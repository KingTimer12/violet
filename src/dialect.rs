//! What sets one SQL database apart from another for the editor: the
//! statements, keywords, functions and types completion offers on it.
//! [`Family::Other`] gets everything, as before databases were told apart.

use violet_core::string_literal;

use crate::sql;

/// What an explain asks of the server beyond the plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Explain {
    /// Runs the statement and measures it.
    pub analyze: bool,
    /// Postgres: pages hit, read and written per step. Only with analyze.
    pub buffers: bool,
    /// Postgres: a clock reading per step. Off, analyze costs less and
    /// shows rows only; the clock is a good part of its overhead.
    pub timing: bool,
}

impl Explain {
    /// The plan alone.
    pub const PLAN: Self = Self {
        analyze: false,
        buffers: true,
        timing: true,
    };
}

impl Default for Explain {
    fn default() -> Self {
        Self::PLAN
    }
}

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

    /// The statement that explains `sql` in a form the plan view reads:
    /// JSON on Postgres and MySQL, a text tree for MySQL's analyze,
    /// `QUERY PLAN` rows on SQLite. `None`
    /// where the database has no such form: SQLite cannot analyze and
    /// other databases are not known.
    pub fn explain_tree(self, sql: &str, explain: &Explain) -> Option<String> {
        match (self, explain.analyze) {
            (Self::Postgres, false) => Some(format!("EXPLAIN (FORMAT JSON) {sql}")),
            (Self::Postgres, true) => {
                let mut options = vec!["ANALYZE"];
                if explain.buffers {
                    options.push("BUFFERS");
                }
                if !explain.timing {
                    options.push("TIMING OFF");
                }
                options.push("FORMAT JSON");
                Some(format!("EXPLAIN ({}) {sql}", options.join(", ")))
            }
            (Self::MySql, false) => Some(format!("EXPLAIN FORMAT=JSON {sql}")),
            // Its only analyze is a text tree, which the plan view parses.
            (Self::MySql, true) => Some(format!("EXPLAIN ANALYZE {sql}")),
            (Self::Sqlite, false) => Some(format!("EXPLAIN QUERY PLAN {sql}")),
            _ => None,
        }
    }

    /// `explain` wrapped so that what the statement wrote is undone: its
    /// own transaction, or a savepoint when one is `open` already, which
    /// then stays as it was. `None` where analyzing a write is not
    /// possible: no tree form, or no way to undo.
    pub fn explain_rolled_back(self, sql: &str, explain: &Explain, open: bool) -> Option<String> {
        if !explain.analyze || !matches!(self, Self::Postgres | Self::MySql) {
            return None;
        }
        let inner = self.explain_tree(sql, explain)?;
        let begin = if self == Self::MySql {
            "START TRANSACTION"
        } else {
            "BEGIN"
        };
        Some(if open {
            format!(
                "SAVEPOINT violet_explain;\n{inner};\nROLLBACK TO SAVEPOINT violet_explain;\nRELEASE SAVEPOINT violet_explain"
            )
        } else {
            format!("{begin};\n{inner};\nROLLBACK")
        })
    }

    /// The query that lists a table's indexes: a row per index, its name
    /// and its columns in order, separated by commas. `None` where the
    /// database's catalog is not known, or a name cannot be sent safely.
    pub fn index_query(self, schema: &str, table: &str) -> Option<String> {
        let (schema, table) = (string_literal(schema)?, string_literal(table)?);
        match self {
            Self::Postgres => Some(format!(
                "SELECT c.relname, array_to_string(ARRAY(SELECT pg_catalog.pg_get_indexdef(i.indexrelid, k + 1, true) \
                 FROM generate_series(0, i.indnkeyatts - 1) k ORDER BY k), ',') \
                 FROM pg_catalog.pg_index i \
                 JOIN pg_catalog.pg_class c ON c.oid = i.indexrelid \
                 JOIN pg_catalog.pg_class t ON t.oid = i.indrelid \
                 JOIN pg_catalog.pg_namespace n ON n.oid = t.relnamespace \
                 WHERE n.nspname = {schema} AND t.relname = {table}"
            )),
            Self::MySql => Some(format!(
                "SELECT index_name, GROUP_CONCAT(column_name ORDER BY seq_in_index) \
                 FROM information_schema.statistics \
                 WHERE table_schema = {schema} AND table_name = {table} AND column_name IS NOT NULL \
                 GROUP BY index_name"
            )),
            Self::Sqlite => Some(format!(
                "SELECT il.name, (SELECT group_concat(name, ',') FROM \
                 (SELECT name FROM pragma_index_info(il.name, {schema}) ORDER BY seqno)) \
                 FROM pragma_index_list({table}, {schema}) il"
            )),
            Self::Other => None,
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

    /// The arguments of a built-in `function`, as written between its
    /// parentheses, when its signature is known here; empty when it takes
    /// none.
    pub fn signature(self, function: &str) -> Option<&'static str> {
        let name = function.to_ascii_lowercase();
        let own = match self {
            Self::Postgres => match name.as_str() {
                "age" => Some("timestamp, timestamp"),
                "array_length" => Some("array, dimension"),
                "current_setting" => Some("name"),
                "date_trunc" => Some("field, source"),
                "extract" => Some("field FROM source"),
                "generate_series" => Some("start, stop, step"),
                "json_agg" | "jsonb_agg" | "array_agg" => Some("expression"),
                "json_build_object" | "jsonb_build_object" => Some("key, value"),
                "regexp_replace" => Some("string, pattern, replacement"),
                "split_part" => Some("string, delimiter, position"),
                "string_agg" => Some("expression, delimiter"),
                "substring" => Some("string FROM start FOR length"),
                "to_char" => Some("value, format"),
                "to_timestamp" => Some("text, format"),
                "unnest" => Some("array"),
                "gen_random_uuid" | "now" => Some(""),
                _ => None,
            },
            Self::MySql => match name.as_str() {
                "concat_ws" => Some("separator, string, string"),
                "date_add" | "date_sub" => Some("date, INTERVAL value unit"),
                "date_format" => Some("date, format"),
                "datediff" => Some("date1, date2"),
                "extract" => Some("unit FROM date"),
                "group_concat" => Some("expression SEPARATOR ','"),
                "if" => Some("condition, then, else"),
                "ifnull" => Some("value, fallback"),
                "json_arrayagg" => Some("expression"),
                "json_extract" => Some("document, path"),
                "json_object" => Some("key, value"),
                "json_objectagg" => Some("key, value"),
                "str_to_date" => Some("string, format"),
                "substring" => Some("string, start, length"),
                "unix_timestamp" => Some("date"),
                "curdate" | "last_insert_id" | "now" | "uuid" => Some(""),
                _ => None,
            },
            Self::Sqlite => match name.as_str() {
                "date" | "datetime" | "julianday" => Some("time, modifier"),
                "group_concat" => Some("expression, separator"),
                "ifnull" => Some("value, fallback"),
                "iif" => Some("condition, then, else"),
                "instr" => Some("string, substring"),
                "json_extract" => Some("document, path"),
                "json_group_array" => Some("expression"),
                "json_group_object" => Some("key, value"),
                "printf" => Some("format, value"),
                "strftime" => Some("format, time, modifier"),
                "substr" => Some("string, start, length"),
                "total" => Some("expression"),
                "typeof" => Some("value"),
                "last_insert_rowid" | "random" => Some(""),
                _ => None,
            },
            Self::Other => match name.as_str() {
                "array_agg" | "json_agg" => Some("expression"),
                "date_trunc" => Some("field, source"),
                "extract" => Some("field FROM source"),
                "string_agg" => Some("expression, delimiter"),
                "substring" => Some("string, start, length"),
                "to_char" => Some("value, format"),
                "greatest" | "least" | "concat" => Some("value, value"),
                "ceil" | "floor" => Some("number"),
                "now" => Some(""),
                _ => None,
            },
        };
        if own.is_some() {
            return own;
        }
        Some(match name.as_str() {
            "abs" | "ceil" | "floor" => "number",
            "avg" | "count" | "max" | "min" | "sum" => "expression",
            "cast" => "expression AS type",
            "coalesce" | "concat" | "greatest" | "least" => "value, value",
            "lag" | "lead" => "value, offset, default",
            "length" | "lower" | "trim" | "upper" => "string",
            "nullif" => "value1, value2",
            "replace" => "string, from, to",
            "round" => "number, decimals",
            "dense_rank" | "rank" | "row_number" => "",
            _ => return None,
        })
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
    fn built_in_functions_have_signatures_per_database() {
        assert_eq!(
            Family::Postgres.signature("DATE_TRUNC"),
            Some("field, source")
        );
        assert_eq!(Family::MySql.signature("ifnull"), Some("value, fallback"));
        assert_eq!(
            Family::Sqlite.signature("substr"),
            Some("string, start, length")
        );
        assert_eq!(Family::Postgres.signature("lower"), Some("string"));
        // Takes no arguments, or is not known.
        assert_eq!(Family::Postgres.signature("now"), Some(""));
        assert_eq!(Family::MySql.signature("my_udf"), None);
        // Every built-in the family lists has one but a few.
        for family in [
            Family::Postgres,
            Family::MySql,
            Family::Sqlite,
            Family::Other,
        ] {
            for function in family.functions() {
                assert!(
                    family.signature(function).is_some(),
                    "{family:?} {function}"
                );
            }
        }
    }

    #[test]
    fn explains_as_a_tree_where_the_database_can() {
        let explain = |family: Family, analyze| {
            let options = Explain {
                analyze,
                ..Explain::PLAN
            };
            family.explain_tree("SELECT 1", &options)
        };
        assert_eq!(
            explain(Family::Postgres, false).as_deref(),
            Some("EXPLAIN (FORMAT JSON) SELECT 1")
        );
        assert_eq!(
            explain(Family::Postgres, true).as_deref(),
            Some("EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) SELECT 1")
        );
        assert_eq!(
            explain(Family::MySql, false).as_deref(),
            Some("EXPLAIN FORMAT=JSON SELECT 1")
        );
        assert_eq!(
            explain(Family::Sqlite, false).as_deref(),
            Some("EXPLAIN QUERY PLAN SELECT 1")
        );
        assert_eq!(
            explain(Family::MySql, true).as_deref(),
            Some("EXPLAIN ANALYZE SELECT 1")
        );
        assert_eq!(explain(Family::Sqlite, true), None);
        assert_eq!(explain(Family::Other, false), None);
    }

    #[test]
    fn postgres_analyze_takes_buffers_and_timing() {
        let lean = Explain {
            analyze: true,
            buffers: false,
            timing: false,
        };
        assert_eq!(
            Family::Postgres.explain_tree("SELECT 1", &lean).as_deref(),
            Some("EXPLAIN (ANALYZE, TIMING OFF, FORMAT JSON) SELECT 1")
        );
        // Without analyze there is nothing to count or time.
        let plan = Explain {
            buffers: false,
            ..Explain::PLAN
        };
        assert_eq!(
            Family::Postgres.explain_tree("SELECT 1", &plan).as_deref(),
            Some("EXPLAIN (FORMAT JSON) SELECT 1")
        );
    }

    #[test]
    fn analyzing_a_write_is_undone() {
        let analyze = Explain {
            analyze: true,
            ..Explain::PLAN
        };
        let run = |open| Family::Postgres.explain_rolled_back("DELETE FROM t", &analyze, open);
        assert_eq!(
            run(false).as_deref(),
            Some("BEGIN;\nEXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) DELETE FROM t;\nROLLBACK")
        );
        let inside = run(true).expect("a script");
        assert!(inside.starts_with("SAVEPOINT violet_explain;"));
        assert!(inside.contains("ROLLBACK TO SAVEPOINT violet_explain"));
        assert_eq!(
            Family::MySql
                .explain_rolled_back("DELETE FROM t", &analyze, false)
                .as_deref(),
            Some("START TRANSACTION;\nEXPLAIN ANALYZE DELETE FROM t;\nROLLBACK")
        );
        // Nothing to undo without analyze, and SQLite has no analyze.
        assert_eq!(
            Family::Postgres.explain_rolled_back("DELETE FROM t", &Explain::PLAN, false),
            None
        );
        assert_eq!(
            Family::Sqlite.explain_rolled_back("DELETE FROM t", &analyze, false),
            None
        );
    }

    #[test]
    fn lists_indexes_per_database() {
        let pg = Family::Postgres
            .index_query("public", "orders")
            .expect("query");
        assert!(pg.contains("pg_index") && pg.contains("t.relname = 'orders'"));
        let my = Family::MySql.index_query("shop", "orders").expect("query");
        assert!(
            my.contains("information_schema.statistics") && my.contains("table_schema = 'shop'")
        );
        let lite = Family::Sqlite.index_query("main", "o'x").expect("query");
        assert!(lite.contains("pragma_index_list('o''x', 'main')"));
        assert_eq!(Family::Other.index_query("a", "b"), None);
        assert_eq!(Family::Postgres.index_query("a", "b\\c"), None);
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
