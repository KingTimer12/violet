//! What a database keeps about one table: its size, rows, the indexes and
//! how often they are used, and when it was last vacuumed or analyzed.

use violet_core::{ResultSet, string_literal};

use crate::dialect::Family;
use crate::health::{self, Table};

/// One index of a table.
#[derive(Clone, Debug, PartialEq)]
pub struct IndexStat {
    pub name: String,
    /// Times the planner read it, since statistics were reset; `None` where
    /// the database does not count.
    pub scans: Option<f64>,
    pub bytes: Option<f64>,
}

/// A table's statistics.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TableStats {
    pub total_bytes: Option<f64>,
    pub table_bytes: Option<f64>,
    pub index_bytes: Option<f64>,
    /// The planner's row count, not an exact one.
    pub rows: Option<f64>,
    pub dead_rows: Option<f64>,
    pub last_vacuum: Option<String>,
    pub last_analyze: Option<String>,
    /// When the table's data last changed, where the database says.
    pub last_change: Option<String>,
    pub indexes: Vec<IndexStat>,
}

impl Family {
    /// What the database cannot tell of a table, and why.
    pub fn stats_gaps(self) -> &'static [&'static str] {
        match self {
            Self::Postgres => &[],
            Self::MySql => &[
                "MySQL does not record when a table was last vacuumed; InnoDB purges on its own.",
                "Index sizes are not exposed without reading mysql.innodb_index_stats.",
            ],
            Self::Sqlite => &[
                "SQLite keeps no index usage counters, row estimates or vacuum times; sizes need the dbstat virtual table.",
            ],
            Self::Other => &["Violet does not know this database's catalog."],
        }
    }

    /// Two statements, a row of figures then a row per index, for the table
    /// `schema.table`. `None` where the catalog is not known or a name
    /// cannot be sent safely.
    pub fn table_stats_query(self, schema: &str, table: &str) -> Option<String> {
        let (s, t) = (string_literal(schema)?, string_literal(table)?);
        match self {
            Self::Postgres => Some(format!(
                "SELECT pg_total_relation_size(c.oid) AS total_bytes, pg_relation_size(c.oid) AS table_bytes, \
                 pg_indexes_size(c.oid) AS index_bytes, c.reltuples::bigint AS est_rows, \
                 st.n_dead_tup AS dead_rows, \
                 greatest(st.last_vacuum, st.last_autovacuum)::text AS last_vacuum, \
                 greatest(st.last_analyze, st.last_autoanalyze)::text AS last_analyze \
                 FROM pg_catalog.pg_class c \
                 JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
                 LEFT JOIN pg_catalog.pg_stat_all_tables st ON st.relid = c.oid \
                 WHERE n.nspname = {s} AND c.relname = {t}; \
                 SELECT i.indexrelname AS name, i.idx_scan AS scans, pg_relation_size(i.indexrelid) AS bytes \
                 FROM pg_catalog.pg_stat_all_indexes i \
                 WHERE i.schemaname = {s} AND i.relname = {t} ORDER BY i.idx_scan, i.indexrelname"
            )),
            Self::MySql => Some(format!(
                "SELECT data_length + index_length AS total_bytes, data_length AS table_bytes, \
                 index_length AS index_bytes, table_rows AS est_rows, data_free AS dead_rows, \
                 update_time AS last_change \
                 FROM information_schema.tables WHERE table_schema = {s} AND table_name = {t}; \
                 SELECT index_name AS name, rows_selected AS scans, NULL AS bytes \
                 FROM sys.schema_index_statistics WHERE table_schema = {s} AND table_name = {t} \
                 ORDER BY rows_selected, index_name"
            )),
            Self::Sqlite => Some(format!(
                "SELECT SUM(pgsize) AS total_bytes, \
                 SUM(CASE WHEN name = {t} THEN pgsize END) AS table_bytes, \
                 SUM(CASE WHEN name <> {t} THEN pgsize END) AS index_bytes \
                 FROM dbstat WHERE name = {t} OR name IN (SELECT name FROM pragma_index_list({t}, {s})); \
                 SELECT name, NULL AS scans, \
                 (SELECT SUM(pgsize) FROM dbstat d WHERE d.name = il.name) AS bytes \
                 FROM pragma_index_list({t}, {s}) il ORDER BY name"
            )),
            Self::Other => None,
        }
    }
}

fn number(table: &Table, row: usize, name: &str) -> Option<f64> {
    table.cell(row, name)?.trim().parse().ok()
}

fn text(table: &Table, row: usize, name: &str) -> Option<String> {
    table
        .cell(row, name)
        .filter(|cell| !cell.is_empty())
        .map(str::to_owned)
}

impl TableStats {
    /// Reads the answers of [`Family::table_stats_query`].
    pub fn from_sets(sets: &[ResultSet]) -> Option<Self> {
        let figures = health::table(sets.first()?);
        if figures.rows.is_empty() {
            return None;
        }
        let mut stats = Self {
            total_bytes: number(&figures, 0, "total_bytes"),
            table_bytes: number(&figures, 0, "table_bytes"),
            index_bytes: number(&figures, 0, "index_bytes"),
            rows: number(&figures, 0, "est_rows").filter(|rows| *rows >= 0.0),
            dead_rows: number(&figures, 0, "dead_rows"),
            last_vacuum: text(&figures, 0, "last_vacuum"),
            last_analyze: text(&figures, 0, "last_analyze"),
            last_change: text(&figures, 0, "last_change"),
            indexes: Vec::new(),
        };
        if let Some(set) = sets.get(1) {
            let indexes = health::table(set);
            stats.indexes = (0..indexes.rows.len())
                .filter_map(|row| {
                    Some(IndexStat {
                        name: text(&indexes, row, "name")?,
                        scans: number(&indexes, row, "scans"),
                        bytes: number(&indexes, row, "bytes"),
                    })
                })
                .collect();
        }
        Some(stats)
    }

    /// Indexes nothing has read: they cost writes and space for no gain
    /// (unless the statistics were reset lately, or it enforces a key).
    pub fn unused(&self) -> Vec<&IndexStat> {
        self.indexes
            .iter()
            .filter(|index| index.scans == Some(0.0))
            .collect()
    }

    /// Dead rows as a share of live and dead, in Postgres.
    pub fn dead_share(&self) -> Option<f64> {
        let (rows, dead) = (self.rows?, self.dead_rows?);
        (rows + dead > 0.0).then(|| dead / (rows + dead))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use violet_core::{ColumnMeta, Rows};

    fn set(columns: &[&str], rows: &[&[Option<&str>]]) -> ResultSet {
        let mut grid = Rows::new(columns.len());
        for row in rows {
            grid.push(row.iter().copied());
        }
        ResultSet {
            columns: columns
                .iter()
                .map(|name| ColumnMeta {
                    name: (*name).to_owned(),
                    type_name: None,
                    numeric: false,
                })
                .collect(),
            rows: grid,
            ..ResultSet::default()
        }
    }

    #[test]
    fn reads_postgres_table_statistics() {
        let figures = set(
            &[
                "total_bytes",
                "table_bytes",
                "index_bytes",
                "est_rows",
                "dead_rows",
                "last_vacuum",
                "last_analyze",
            ],
            &[&[
                Some("10000"),
                Some("8000"),
                Some("2000"),
                Some("900"),
                Some("100"),
                Some("2026-10-01 10:00:00+00"),
                None,
            ]],
        );
        let indexes = set(
            &["name", "scans", "bytes"],
            &[
                &[Some("by_email"), Some("0"), Some("1500")],
                &[Some("orders_pkey"), Some("5000"), Some("500")],
            ],
        );
        let stats = TableStats::from_sets(&[figures, indexes]).expect("stats");
        assert_eq!(stats.total_bytes, Some(10000.0));
        assert_eq!(stats.rows, Some(900.0));
        assert_eq!(stats.dead_share(), Some(0.1));
        assert_eq!(stats.last_vacuum.as_deref(), Some("2026-10-01 10:00:00+00"));
        assert_eq!(stats.last_analyze, None);
        assert_eq!(stats.indexes.len(), 2);
        assert_eq!(
            stats
                .unused()
                .iter()
                .map(|i| i.name.as_str())
                .collect::<Vec<_>>(),
            ["by_email"]
        );
    }

    #[test]
    fn a_missing_table_has_no_statistics() {
        let empty = set(&["total_bytes"], &[]);
        assert!(TableStats::from_sets(&[empty]).is_none());
        assert!(TableStats::from_sets(&[]).is_none());
        // An unknown row count (-1 before the first analyze) is no count.
        let figures = set(&["est_rows"], &[&[Some("-1")]]);
        assert_eq!(TableStats::from_sets(&[figures]).and_then(|s| s.rows), None);
    }

    #[test]
    fn asks_each_database_its_own_way() {
        let pg = Family::Postgres
            .table_stats_query("public", "orders")
            .expect("query");
        assert!(pg.contains("pg_stat_all_tables") && pg.contains("c.relname = 'orders'"));
        assert!(pg.contains("pg_stat_all_indexes"));
        let my = Family::MySql
            .table_stats_query("shop", "orders")
            .expect("query");
        assert!(
            my.contains("information_schema.tables") && my.contains("sys.schema_index_statistics")
        );
        let lite = Family::Sqlite
            .table_stats_query("main", "orders")
            .expect("query");
        assert!(lite.contains("dbstat") && lite.contains("pragma_index_list('orders', 'main')"));
        assert_eq!(Family::Other.table_stats_query("a", "b"), None);
        assert_eq!(Family::Postgres.table_stats_query("a", "b\\c"), None);
        assert!(!Family::Sqlite.stats_gaps().is_empty());
        assert!(Family::Postgres.stats_gaps().is_empty());
    }
}
