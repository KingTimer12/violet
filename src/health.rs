//! What each database tells of its own health: the statements that read
//! it, and what their answers mean. Sessions and locks, slow statements,
//! cache hit ratios, memory, clients. Where a database has nothing to say
//! (SQLite runs inside the app; Redis keeps no per-query statistics) the
//! panel says so instead of showing a blank.

use std::collections::HashMap;

use serde_json::Value;
use violet_core::{Language, ResultSet};

use crate::dialect::Family;

/// What a connection's server is, for the panel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Server {
    Postgres,
    MySql,
    Sqlite,
    Redis,
    Mongo,
    /// Another SQL database: nothing known of its catalog.
    Unknown,
}

impl Server {
    pub fn of(language: Language, family: Family) -> Self {
        match (language, family) {
            (Language::Redis, _) => Self::Redis,
            (Language::Mongodb, _) => Self::Mongo,
            (Language::Sql, Family::Postgres) => Self::Postgres,
            (Language::Sql, Family::MySql) => Self::MySql,
            (Language::Sql, Family::Sqlite) => Self::Sqlite,
            (Language::Sql, Family::Other) => Self::Unknown,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Postgres => "PostgreSQL",
            Self::MySql => "MySQL",
            Self::Sqlite => "SQLite",
            Self::Redis => "Redis",
            Self::Mongo => "MongoDB",
            Self::Unknown => "This database",
        }
    }

    /// What the panel cannot show for this database, and why: the server
    /// does not keep it, or does not expose it to a client.
    pub fn missing(self) -> &'static [(&'static str, &'static str)] {
        match self {
            Self::Sqlite => &[
                (
                    "Sessions and locks",
                    "SQLite runs inside the application: there is no server process with sessions to list or cancel.",
                ),
                (
                    "Slow statements",
                    "SQLite keeps no statement statistics or slow log.",
                ),
            ],
            Self::Redis => &[
                (
                    "Locks",
                    "Redis runs commands one at a time and takes no locks.",
                ),
                (
                    "Per-query plans",
                    "Redis has no query plans; commandstats and the slow log time commands instead.",
                ),
            ],
            Self::MySql => &[(
                "Slow log file",
                "When the slow log goes to a file (log_output=FILE), a client cannot read it; only the mysql.slow_log table form is shown.",
            )],
            Self::Postgres => &[(
                "Table bloat",
                "Postgres does not report bloat without an extension; dead tuples and vacuum times stand in for it.",
            )],
            Self::Mongo => &[],
            Self::Unknown => &[(
                "Server statistics",
                "Violet does not know this database's catalog, so it cannot read its health.",
            )],
        }
    }

    /// The statement that ends a session or a running operation, `id` as
    /// the sessions table lists it; `None` where the server has no such
    /// thing, or `id` is not safe to send. `whole` ends the session, not
    /// only what it runs.
    pub fn stop(self, id: &str, whole: bool) -> Option<String> {
        let safe = !id.is_empty()
            && id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '_' | '-'));
        if !safe {
            return None;
        }
        let number = id.chars().all(|c| c.is_ascii_digit());
        match self {
            Self::Postgres if number => Some(if whole {
                format!("SELECT pg_terminate_backend({id})")
            } else {
                format!("SELECT pg_cancel_backend({id})")
            }),
            Self::MySql if number => Some(if whole {
                format!("KILL {id}")
            } else {
                format!("KILL QUERY {id}")
            }),
            // A client has no query to cancel apart from its connection.
            Self::Redis if number && whole => Some(format!("CLIENT KILL ID {id}")),
            Self::Mongo if number => Some(format!("db.killOp({id})")),
            Self::Mongo => Some(format!("db.killOp(\"{id}\")")),
            _ => None,
        }
    }

    /// Whether [`Self::stop`] can cancel only the running query.
    pub fn cancels_query(self) -> bool {
        matches!(self, Self::Postgres | Self::MySql | Self::Mongo)
    }
}

/// One thing the panel shows: what asks for it, and how its answer reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Section {
    pub id: &'static str,
    pub title: &'static str,
    /// What to run.
    pub sql: String,
    /// Tried when the first fails, such as an older server's column names.
    pub fallback: Option<String>,
    /// Said when it fails: what the server needs for this to work.
    pub needs: &'static str,
    /// The column that names a session or operation to stop.
    pub stoppable: Option<&'static str>,
}

fn section(id: &'static str, title: &'static str, sql: &str, needs: &'static str) -> Section {
    Section {
        id,
        title,
        sql: sql.to_owned(),
        fallback: None,
        needs,
        stoppable: None,
    }
}

/// The sections of `server`'s panel, the overview first.
pub fn sections(server: Server) -> Vec<Section> {
    match server {
        Server::Postgres => {
            let mut statements = section(
                "statements",
                "Top statements",
                "SELECT left(query, 200) AS query, calls, round(total_exec_time::numeric, 1) AS total_ms, \
                 round(mean_exec_time::numeric, 2) AS mean_ms, rows \
                 FROM pg_stat_statements ORDER BY total_exec_time DESC LIMIT 20",
                "The pg_stat_statements extension: add it to shared_preload_libraries, then CREATE EXTENSION pg_stat_statements.",
            );
            // Before Postgres 13 the columns were `total_time` and `mean_time`.
            statements.fallback = Some(
                "SELECT left(query, 200) AS query, calls, round(total_time::numeric, 1) AS total_ms, \
                 round(mean_time::numeric, 2) AS mean_ms, rows \
                 FROM pg_stat_statements ORDER BY total_time DESC LIMIT 20"
                    .to_owned(),
            );
            let mut sessions = section(
                "sessions",
                "Sessions",
                "SELECT pid, usename AS user, application_name AS application, client_addr::text AS client, \
                 state, wait_event_type AS wait_type, wait_event, \
                 round(extract(epoch FROM now() - query_start)::numeric, 1) AS running_seconds, \
                 left(query, 200) AS query \
                 FROM pg_stat_activity WHERE pid <> pg_backend_pid() AND datname = current_database() \
                 ORDER BY query_start NULLS LAST",
                "",
            );
            sessions.stoppable = Some("pid");
            vec![
                section(
                    "overview",
                    "Overview",
                    "SELECT d.numbackends, \
                     (SELECT setting::bigint FROM pg_settings WHERE name = 'max_connections') AS max_connections, \
                     d.xact_commit, d.xact_rollback, d.blks_read, d.blks_hit, d.deadlocks, d.temp_files, d.temp_bytes, \
                     pg_database_size(d.datname) AS size_bytes, \
                     extract(epoch FROM now() - pg_postmaster_start_time())::bigint AS uptime_seconds \
                     FROM pg_stat_database d WHERE d.datname = current_database()",
                    "",
                ),
                sessions,
                section(
                    "locks",
                    "Locks",
                    "SELECT blocked.pid AS blocked_pid, left(blocked.query, 120) AS blocked_query, \
                     blocking.pid AS blocking_pid, left(blocking.query, 120) AS blocking_query, \
                     round(extract(epoch FROM now() - blocked.query_start)::numeric, 1) AS waiting_seconds \
                     FROM pg_stat_activity blocked \
                     CROSS JOIN LATERAL unnest(pg_blocking_pids(blocked.pid)) AS b(pid) \
                     JOIN pg_stat_activity blocking ON blocking.pid = b.pid",
                    "Postgres 9.6 or later.",
                ),
                statements,
                section(
                    "tables",
                    "Tables and vacuum",
                    "SELECT schemaname AS schema, relname AS table, n_live_tup AS live_rows, n_dead_tup AS dead_rows, \
                     round(100.0 * n_dead_tup / greatest(n_live_tup + n_dead_tup, 1), 1) AS dead_pct, \
                     last_vacuum, last_autovacuum, last_analyze, last_autoanalyze, \
                     pg_total_relation_size(relid) AS size_bytes \
                     FROM pg_stat_user_tables ORDER BY n_dead_tup DESC LIMIT 50",
                    "",
                ),
                section(
                    "indexes",
                    "Unused indexes",
                    "SELECT s.schemaname AS schema, s.relname AS table, s.indexrelname AS index, s.idx_scan AS scans, \
                     pg_relation_size(s.indexrelid) AS size_bytes \
                     FROM pg_stat_user_indexes s JOIN pg_index i ON i.indexrelid = s.indexrelid \
                     WHERE s.idx_scan = 0 AND NOT i.indisunique AND NOT i.indisprimary \
                     ORDER BY pg_relation_size(s.indexrelid) DESC LIMIT 50",
                    "",
                ),
            ]
        }
        Server::MySql => {
            let mut sessions = section("sessions", "Sessions", "SHOW FULL PROCESSLIST", "");
            sessions.stoppable = Some("Id");
            vec![
                section(
                    "overview",
                    "Overview",
                    "SHOW GLOBAL STATUS; \
                     SHOW GLOBAL VARIABLES WHERE Variable_name IN \
                     ('max_connections', 'slow_query_log', 'long_query_time', 'innodb_buffer_pool_size', 'log_output')",
                    "",
                ),
                sessions,
                section(
                    "locks",
                    "Locks",
                    "SELECT wait_started, wait_age_secs AS waiting_seconds, locked_table, \
                     waiting_pid, LEFT(waiting_query, 120) AS waiting_query, \
                     blocking_pid, LEFT(blocking_query, 120) AS blocking_query \
                     FROM sys.innodb_lock_waits",
                    "The sys schema (MySQL 5.7 or later) and InnoDB.",
                ),
                section(
                    "statements",
                    "Top statements",
                    "SELECT LEFT(DIGEST_TEXT, 200) AS query, COUNT_STAR AS calls, \
                     ROUND(SUM_TIMER_WAIT / 1000000000, 1) AS total_ms, ROUND(AVG_TIMER_WAIT / 1000000000, 2) AS mean_ms, \
                     SUM_ROWS_EXAMINED AS rows_examined, SUM_ROWS_SENT AS rows_sent, SUM_NO_INDEX_USED AS no_index \
                     FROM performance_schema.events_statements_summary_by_digest \
                     ORDER BY SUM_TIMER_WAIT DESC LIMIT 20",
                    "performance_schema enabled (performance_schema=ON in the server's configuration).",
                ),
                section(
                    "slow",
                    "Slow log",
                    "SELECT start_time, user_host, query_time, lock_time, rows_sent, rows_examined, \
                     LEFT(sql_text, 200) AS query \
                     FROM mysql.slow_log ORDER BY start_time DESC LIMIT 50",
                    "slow_query_log=ON with log_output=TABLE; a log kept in a file cannot be read from here.",
                ),
                section(
                    "tables",
                    "Tables",
                    "SELECT table_schema AS `schema`, table_name AS `table`, engine, table_rows AS `rows`, \
                     data_length, index_length, data_free, update_time \
                     FROM information_schema.tables \
                     WHERE table_schema NOT IN ('mysql', 'sys', 'information_schema', 'performance_schema') \
                     ORDER BY data_length + index_length DESC LIMIT 50",
                    "",
                ),
                section(
                    "indexes",
                    "Unused indexes",
                    "SELECT object_schema AS `schema`, object_name AS `table`, index_name AS `index` \
                     FROM sys.schema_unused_indexes",
                    "The sys schema, with performance_schema running since a while.",
                ),
            ]
        }
        Server::Redis => {
            let mut clients = section("clients", "Clients", "CLIENT LIST", "");
            clients.stoppable = Some("id");
            vec![
                section("overview", "Overview", "INFO", ""),
                section("commands", "Commands", "INFO commandstats", ""),
                section("slowlog", "Slow log", "SLOWLOG GET 50", ""),
                clients,
                section(
                    "latency",
                    "Latency",
                    "LATENCY LATEST",
                    "latency-monitor-threshold above 0 in the server's configuration.",
                ),
            ]
        }
        Server::Mongo => {
            let mut operations = section(
                "operations",
                "Operations",
                "db.currentOp()",
                "The inprog action on the cluster.",
            );
            operations.stoppable = Some("opid");
            vec![
                section("overview", "Overview", "db.serverStatus()", ""),
                operations,
                section(
                    "profiler",
                    "Profiler",
                    "db.system.profile.find().sort({ ts: -1 }).limit(50)",
                    "Profiling on: db.setProfilingLevel(1, { slowms: 100 }) records operations slower than 100 ms.",
                ),
            ]
        }
        Server::Sqlite => vec![
            section(
                "overview",
                "Overview",
                "PRAGMA page_count; PRAGMA page_size; PRAGMA freelist_count; PRAGMA journal_mode; \
                 PRAGMA cache_size; PRAGMA auto_vacuum",
                "",
            ),
            section(
                "tables",
                "Tables",
                "SELECT name, SUM(pgsize) AS size_bytes, COUNT(*) AS pages FROM dbstat \
                 GROUP BY name ORDER BY size_bytes DESC LIMIT 50",
                "SQLite built with the dbstat virtual table (SQLITE_ENABLE_DBSTAT_VTAB).",
            ),
        ],
        Server::Unknown => Vec::new(),
    }
}

/// A grid of text: what a section shows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Table {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

impl Table {
    pub fn column(&self, name: &str) -> Option<usize> {
        self.columns
            .iter()
            .position(|column| column.eq_ignore_ascii_case(name))
    }

    /// `name`'s cell in row `row`.
    pub fn cell(&self, row: usize, name: &str) -> Option<&str> {
        self.rows
            .get(row)?
            .get(self.column(name)?)
            .map(String::as_str)
    }
}

/// A result set as a table, `NULL` as an empty cell.
pub fn table(set: &ResultSet) -> Table {
    Table {
        columns: set.columns.iter().map(|c| c.name.clone()).collect(),
        rows: set
            .rows
            .iter()
            .map(|row| {
                row.iter()
                    .map(|cell| cell.unwrap_or_default().to_owned())
                    .collect()
            })
            .collect(),
    }
}

/// The first two columns of a set as `name -> value`.
pub fn pairs(sets: &[ResultSet]) -> HashMap<String, String> {
    let mut found = HashMap::new();
    for set in sets {
        for row in set.rows.iter() {
            if let (Some(Some(name)), Some(Some(value))) = (row.get(0), row.get(1)) {
                found.insert(name.to_owned(), value.to_owned());
            }
        }
    }
    found
}

/// A figure to look at, and whether it is a cause for worry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Info,
    Good,
    Warn,
    Bad,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Card {
    pub label: &'static str,
    pub value: String,
    pub level: Level,
    /// What the figure means, and when to worry.
    pub hint: &'static str,
}

fn card(label: &'static str, value: impl Into<String>, level: Level, hint: &'static str) -> Card {
    Card {
        label,
        value: value.into(),
        level,
        hint,
    }
}

fn number(text: Option<&String>) -> Option<f64> {
    text?.trim().parse().ok()
}

/// Bytes as `1.5 GiB`.
pub fn bytes(value: f64) -> String {
    let mut size = value;
    for unit in ["B", "KiB", "MiB", "GiB", "TiB"] {
        if size.abs() < 1024.0 || unit == "TiB" {
            return if unit == "B" {
                format!("{size:.0} B")
            } else {
                format!("{size:.1} {unit}")
            };
        }
        size /= 1024.0;
    }
    unreachable!()
}

/// Seconds as `3 d 4 h`, `5 min`.
pub fn uptime(seconds: f64) -> String {
    let s = seconds.max(0.0) as u64;
    match s {
        0..=59 => format!("{s} s"),
        60..=3599 => format!("{} min", s / 60),
        3600..=86_399 => format!("{} h {} min", s / 3600, s % 3600 / 60),
        _ => format!("{} d {} h", s / 86_400, s % 86_400 / 3600),
    }
}

fn percent(ratio: f64) -> String {
    format!("{:.1}%", ratio * 100.0)
}

/// The ratio of hits, flagged when low: a cache above `good` is fine, one
/// under `bad` is a problem.
fn hit_level(ratio: f64, good: f64, bad: f64) -> Level {
    if ratio >= good {
        Level::Good
    } else if ratio >= bad {
        Level::Warn
    } else {
        Level::Bad
    }
}

fn usage_level(used: f64, limit: f64) -> Level {
    if limit <= 0.0 {
        Level::Info
    } else if used / limit >= 0.9 {
        Level::Bad
    } else if used / limit >= 0.7 {
        Level::Warn
    } else {
        Level::Info
    }
}

/// The figures of the overview for `server`, from what its overview query
/// returned.
pub fn overview(server: Server, sets: &[ResultSet]) -> Vec<Card> {
    match server {
        Server::Postgres => postgres_overview(sets),
        Server::MySql => mysql_overview(&pairs(sets)),
        Server::Redis => redis_overview(&redis_info(sets)),
        Server::Mongo => mongo_overview(sets),
        Server::Sqlite => sqlite_overview(sets),
        Server::Unknown => Vec::new(),
    }
}

fn postgres_overview(sets: &[ResultSet]) -> Vec<Card> {
    let Some(set) = sets.first() else {
        return Vec::new();
    };
    let table = table(set);
    let value = |name: &str| number(table.cell(0, name).map(str::to_owned).as_ref());
    let mut cards = Vec::new();
    if let (Some(now), Some(max)) = (value("numbackends"), value("max_connections")) {
        cards.push(card(
            "Connections",
            format!("{now:.0} / {max:.0}"),
            usage_level(now, max),
            "Sessions on this database against the server's max_connections.",
        ));
    }
    if let (Some(hit), Some(read)) = (value("blks_hit"), value("blks_read"))
        && hit + read > 0.0
    {
        let ratio = hit / (hit + read);
        cards.push(card(
            "Cache hit ratio",
            percent(ratio),
            hit_level(ratio, 0.99, 0.9),
            "Pages found in shared buffers. Under 99% on a busy database: more memory may help.",
        ));
    }
    if let Some(size) = value("size_bytes") {
        cards.push(card(
            "Size",
            bytes(size),
            Level::Info,
            "On disk, with its indexes.",
        ));
    }
    if let Some(seconds) = value("uptime_seconds") {
        cards.push(card(
            "Uptime",
            uptime(seconds),
            Level::Info,
            "Since the server started.",
        ));
    }
    if let (Some(commit), Some(rollback)) = (value("xact_commit"), value("xact_rollback"))
        && commit + rollback > 0.0
    {
        let ratio = rollback / (commit + rollback);
        cards.push(card(
            "Rollbacks",
            percent(ratio),
            if ratio > 0.1 {
                Level::Warn
            } else {
                Level::Info
            },
            "Share of transactions rolled back since statistics were reset.",
        ));
    }
    if let Some(deadlocks) = value("deadlocks") {
        cards.push(card(
            "Deadlocks",
            format!("{deadlocks:.0}"),
            if deadlocks > 0.0 {
                Level::Warn
            } else {
                Level::Good
            },
            "Transactions the server aborted to break a lock cycle.",
        ));
    }
    if let (Some(files), Some(size)) = (value("temp_files"), value("temp_bytes")) {
        cards.push(card(
            "Temp files",
            format!("{files:.0} ({})", bytes(size)),
            if files > 0.0 {
                Level::Warn
            } else {
                Level::Good
            },
            "Sorts and hashes that spilled to disk. More work_mem may help.",
        ));
    }
    cards
}

fn mysql_overview(status: &HashMap<String, String>) -> Vec<Card> {
    let value = |name: &str| number(status.get(name));
    let mut cards = Vec::new();
    if let (Some(now), Some(max)) = (value("Threads_connected"), value("max_connections")) {
        cards.push(card(
            "Connections",
            format!("{now:.0} / {max:.0}"),
            usage_level(now, max),
            "Open connections against max_connections.",
        ));
    }
    if let (Some(requests), Some(reads)) = (
        value("Innodb_buffer_pool_read_requests"),
        value("Innodb_buffer_pool_reads"),
    ) && requests > 0.0
    {
        let ratio = 1.0 - reads / requests;
        cards.push(card(
            "Buffer pool hit ratio",
            percent(ratio),
            hit_level(ratio, 0.99, 0.9),
            "Page reads served from the InnoDB buffer pool. Under 99%: a larger pool may help.",
        ));
    }
    if let (Some(questions), Some(up)) = (value("Questions"), value("Uptime"))
        && up > 0.0
    {
        cards.push(card(
            "Queries per second",
            format!("{:.1}", questions / up),
            Level::Info,
            "Statements run since the server started, over its uptime.",
        ));
    }
    if let Some(up) = value("Uptime") {
        cards.push(card(
            "Uptime",
            uptime(up),
            Level::Info,
            "Since the server started.",
        ));
    }
    if let Some(slow) = value("Slow_queries") {
        cards.push(card(
            "Slow queries",
            format!("{slow:.0}"),
            if slow > 0.0 { Level::Warn } else { Level::Good },
            "Statements slower than long_query_time since the server started.",
        ));
    }
    if let (Some(disk), Some(all)) = (
        value("Created_tmp_disk_tables"),
        value("Created_tmp_tables"),
    ) && all > 0.0
    {
        let ratio = disk / all;
        cards.push(card(
            "Temp tables on disk",
            percent(ratio),
            if ratio > 0.25 {
                Level::Warn
            } else {
                Level::Info
            },
            "Temporary tables that did not fit in memory.",
        ));
    }
    if let Some(aborted) = value("Aborted_connects") {
        cards.push(card(
            "Aborted connects",
            format!("{aborted:.0}"),
            if aborted > 0.0 {
                Level::Warn
            } else {
                Level::Good
            },
            "Failed attempts to connect: bad credentials or a network cut.",
        ));
    }
    if let Some(log) = status.get("slow_query_log") {
        let threshold = status
            .get("long_query_time")
            .map_or(String::new(), |t| format!(", over {t} s"));
        cards.push(card(
            "Slow log",
            format!("{}{threshold}", log.to_lowercase()),
            Level::Info,
            "Whether the server records slow statements.",
        ));
    }
    cards
}

/// What `INFO` returned, as `field -> value`.
pub fn redis_info(sets: &[ResultSet]) -> HashMap<String, String> {
    pairs(sets)
}

fn redis_overview(info: &HashMap<String, String>) -> Vec<Card> {
    let value = |name: &str| number(info.get(name));
    let mut cards = Vec::new();
    if let Some(used) = value("used_memory") {
        let max = value("maxmemory").unwrap_or(0.0);
        let detail = if max > 0.0 {
            format!("{} / {}", bytes(used), bytes(max))
        } else {
            format!("{} (no limit)", bytes(used))
        };
        cards.push(card(
            "Memory",
            detail,
            usage_level(used, max),
            "Memory the dataset takes, against maxmemory when it is set.",
        ));
    }
    if let Some(ratio) = value("mem_fragmentation_ratio") {
        cards.push(card(
            "Fragmentation",
            format!("{ratio:.2}"),
            if !(0.9..=1.5).contains(&ratio) { Level::Warn } else { Level::Good },
            "Memory the system gave the process over what the data needs; above 1.5 wastes memory, under 1 means swapping.",
        ));
    }
    if let (Some(hits), Some(misses)) = (value("keyspace_hits"), value("keyspace_misses"))
        && hits + misses > 0.0
    {
        let ratio = hits / (hits + misses);
        cards.push(card(
            "Hit ratio",
            percent(ratio),
            hit_level(ratio, 0.9, 0.5),
            "Key lookups that found their key.",
        ));
    }
    if let Some(ops) = value("instantaneous_ops_per_sec") {
        cards.push(card(
            "Operations per second",
            format!("{ops:.0}"),
            Level::Info,
            "Commands per second over the last moments.",
        ));
    }
    if let Some(clients) = value("connected_clients") {
        let max = value("maxclients").unwrap_or(0.0);
        cards.push(card(
            "Clients",
            if max > 0.0 {
                format!("{clients:.0} / {max:.0}")
            } else {
                format!("{clients:.0}")
            },
            usage_level(clients, max),
            "Connected clients against maxclients.",
        ));
    }
    if let Some(blocked) = value("blocked_clients") {
        cards.push(card(
            "Blocked clients",
            format!("{blocked:.0}"),
            if blocked > 0.0 {
                Level::Warn
            } else {
                Level::Good
            },
            "Clients waiting on a blocking command such as BLPOP.",
        ));
    }
    if let Some(evicted) = value("evicted_keys") {
        cards.push(card(
            "Evicted keys",
            format!("{evicted:.0}"),
            if evicted > 0.0 {
                Level::Warn
            } else {
                Level::Good
            },
            "Keys dropped to stay under maxmemory.",
        ));
    }
    if let Some(up) = value("uptime_in_seconds") {
        cards.push(card(
            "Uptime",
            uptime(up),
            Level::Info,
            "Since the server started.",
        ));
    }
    if let Some(role) = info.get("role") {
        cards.push(card(
            "Role",
            role.as_str(),
            Level::Info,
            "Master or replica.",
        ));
    }
    cards
}

/// Per-command statistics: `cmdstat_get:calls=3,usec=30,usec_per_call=10.00,...`
/// as one row per command, the slowest on average first.
pub fn redis_commands(sets: &[ResultSet]) -> Table {
    let mut rows: Vec<(f64, Vec<String>)> = Vec::new();
    for (name, stats) in pairs(sets) {
        let Some(command) = name.strip_prefix("cmdstat_") else {
            continue;
        };
        let field = |key: &str| {
            stats
                .split(',')
                .find_map(|part| part.strip_prefix(key)?.strip_prefix('='))
                .unwrap_or("")
                .to_owned()
        };
        let per_call: f64 = field("usec_per_call").parse().unwrap_or(0.0);
        rows.push((
            per_call,
            vec![
                command.to_owned(),
                field("calls"),
                field("usec"),
                field("usec_per_call"),
                field("failed_calls"),
                field("rejected_calls"),
            ],
        ));
    }
    rows.sort_by(|a, b| b.0.total_cmp(&a.0));
    Table {
        columns: [
            "command",
            "calls",
            "total_usec",
            "usec_per_call",
            "failed",
            "rejected",
        ]
        .map(String::from)
        .to_vec(),
        rows: rows.into_iter().map(|(_, row)| row).collect(),
    }
}

/// The JSON a cell of the first set holds.
fn first_json(sets: &[ResultSet]) -> Option<Value> {
    let set = sets.first()?;
    set.rows
        .iter()
        .flat_map(|row| row.iter().flatten().map(str::to_owned).collect::<Vec<_>>())
        .find_map(|cell| serde_json::from_str(&cell).ok())
}

fn text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// `SLOWLOG GET`: `[[id, unix time, microseconds, [args...], client, name], ...]`.
pub fn redis_slowlog(sets: &[ResultSet]) -> Table {
    let mut rows = Vec::new();
    if let Some(Value::Array(entries)) = first_json(sets) {
        for entry in entries {
            let Some(fields) = entry.as_array() else {
                continue;
            };
            let at = |index: usize| fields.get(index).map(text).unwrap_or_default();
            let command = fields
                .get(3)
                .and_then(Value::as_array)
                .map(|args| args.iter().map(text).collect::<Vec<_>>().join(" "))
                .unwrap_or_default();
            let micros: f64 = at(2).parse().unwrap_or(0.0);
            rows.push(vec![
                at(0),
                at(1),
                format!("{:.2}", micros / 1000.0),
                command,
                at(4),
            ]);
        }
    }
    Table {
        columns: ["id", "unix_time", "duration_ms", "command", "client"]
            .map(String::from)
            .to_vec(),
        rows,
    }
}

/// `CLIENT LIST`: a line of `key=value` pairs per client.
pub fn redis_clients(sets: &[ResultSet]) -> Table {
    let listing = sets
        .first()
        .and_then(|set| set.rows.get(0, 0)?)
        .unwrap_or_default();
    let columns = ["id", "addr", "name", "age", "idle", "db", "flags", "cmd"];
    let rows = listing
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let fields: HashMap<&str, &str> = line
                .split_whitespace()
                .filter_map(|pair| pair.split_once('='))
                .collect();
            columns
                .iter()
                .map(|key| fields.get(key).copied().unwrap_or("").to_owned())
                .collect()
        })
        .collect();
    Table {
        columns: columns.map(String::from).to_vec(),
        rows,
    }
}

/// `LATENCY LATEST`: `[[event, unix time, latest ms, max ms], ...]`.
pub fn redis_latency(sets: &[ResultSet]) -> Table {
    let mut rows = Vec::new();
    if let Some(Value::Array(events)) = first_json(sets) {
        for event in events {
            if let Some(fields) = event.as_array() {
                rows.push(
                    (0..4)
                        .map(|i| fields.get(i).map(text).unwrap_or_default())
                        .collect(),
                );
            }
        }
    }
    Table {
        columns: ["event", "unix_time", "latest_ms", "max_ms"]
            .map(String::from)
            .to_vec(),
        rows,
    }
}

fn path<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.').try_fold(value, |value, key| value.get(key))
}

fn mongo_overview(sets: &[ResultSet]) -> Vec<Card> {
    let Some(status) = first_json(sets) else {
        return Vec::new();
    };
    let value = |p: &str| {
        path(&status, p).and_then(|v| {
            v.as_f64()
                .or_else(|| v.get("$numberLong")?.as_str()?.parse().ok())
        })
    };
    let mut cards = Vec::new();
    if let (Some(now), Some(free)) = (value("connections.current"), value("connections.available"))
    {
        cards.push(card(
            "Connections",
            format!("{now:.0} / {:.0}", now + free),
            usage_level(now, now + free),
            "Open connections against what the server allows.",
        ));
    }
    let (used, max) = (
        value("wiredTiger.cache.bytes currently in the cache"),
        value("wiredTiger.cache.maximum bytes configured"),
    );
    if let (Some(used), Some(max)) = (used, max) {
        cards.push(card(
            "WiredTiger cache",
            format!("{} / {}", bytes(used), bytes(max)),
            usage_level(used, max),
            "Data in the storage engine's cache against its limit.",
        ));
    }
    if let Some(resident) = value("mem.resident") {
        cards.push(card(
            "Resident memory",
            bytes(resident * 1024.0 * 1024.0),
            Level::Info,
            "Memory the process holds.",
        ));
    }
    let operations: Vec<String> = ["query", "insert", "update", "delete", "command"]
        .iter()
        .filter_map(|op| Some(format!("{op} {:.0}", value(&format!("opcounters.{op}"))?)))
        .collect();
    if !operations.is_empty() {
        cards.push(card(
            "Operations",
            operations.join(" · "),
            Level::Info,
            "Counted since the server started.",
        ));
    }
    if let Some(queued) = value("globalLock.currentQueue.total") {
        cards.push(card(
            "Queued by locks",
            format!("{queued:.0}"),
            if queued > 0.0 {
                Level::Warn
            } else {
                Level::Good
            },
            "Operations waiting for a lock.",
        ));
    }
    if let Some(up) = value("uptime") {
        cards.push(card(
            "Uptime",
            uptime(up),
            Level::Info,
            "Since the server started.",
        ));
    }
    cards
}

/// The operations `db.currentOp()` lists: `{ inprog: [...] }`.
pub fn mongo_operations(sets: &[ResultSet]) -> Table {
    let mut rows = Vec::new();
    if let Some(found) = first_json(sets) {
        let list = found.get("inprog").cloned().unwrap_or(found);
        for op in list.as_array().into_iter().flatten() {
            let field = |p: &str| path(op, p).map(text).unwrap_or_default();
            rows.push(vec![
                field("opid"),
                field("op"),
                field("ns"),
                field("secs_running"),
                field("client"),
                field("command"),
            ]);
        }
    }
    Table {
        columns: ["opid", "op", "ns", "secs_running", "client", "command"]
            .map(String::from)
            .to_vec(),
        rows,
    }
}

/// The documents of `system.profile`, newest first.
pub fn mongo_profile(sets: &[ResultSet]) -> Table {
    let mut docs: Vec<Value> = Vec::new();
    if let Some(set) = sets.first() {
        for row in set.rows.iter() {
            for cell in row.iter().flatten() {
                match serde_json::from_str::<Value>(cell) {
                    Ok(Value::Array(items)) => docs.extend(items),
                    Ok(doc @ Value::Object(_)) => docs.push(doc),
                    _ => {}
                }
            }
        }
    }
    let rows = docs
        .iter()
        .map(|doc| {
            let field = |p: &str| path(doc, p).map(text).unwrap_or_default();
            vec![
                field("ts"),
                field("op"),
                field("ns"),
                field("millis"),
                field("docsExamined"),
                field("nreturned"),
                field("planSummary"),
            ]
        })
        .collect();
    Table {
        columns: [
            "ts",
            "op",
            "ns",
            "millis",
            "docs_examined",
            "returned",
            "plan",
        ]
        .map(String::from)
        .to_vec(),
        rows,
    }
}

fn sqlite_overview(sets: &[ResultSet]) -> Vec<Card> {
    // One set per pragma, one cell each, in the order they were asked.
    let cell =
        |index: usize| -> Option<String> { sets.get(index)?.rows.get(0, 0)?.map(str::to_owned) };
    let value = |index: usize| cell(index).and_then(|text| text.parse::<f64>().ok());
    let mut cards = Vec::new();
    if let (Some(pages), Some(size)) = (value(0), value(1)) {
        cards.push(card(
            "File size",
            bytes(pages * size),
            Level::Info,
            "Pages times page size.",
        ));
        if let Some(free) = value(2)
            && pages > 0.0
        {
            let ratio = free / pages;
            cards.push(card(
                "Free pages",
                format!("{free:.0} ({})", percent(ratio)),
                if ratio > 0.2 {
                    Level::Warn
                } else {
                    Level::Info
                },
                "Space inside the file no table uses. VACUUM gives it back.",
            ));
        }
    }
    if let Some(mode) = cell(3) {
        let level = if mode.eq_ignore_ascii_case("wal") {
            Level::Good
        } else {
            Level::Info
        };
        cards.push(card(
            "Journal mode",
            mode,
            level,
            "WAL lets readers run beside a writer.",
        ));
    }
    if let Some(cache) = value(4) {
        let text = if cache < 0.0 {
            format!("{} KiB", -cache)
        } else {
            format!("{cache:.0} pages")
        };
        cards.push(card(
            "Cache size",
            text,
            Level::Info,
            "Negative values are kibibytes, positive ones pages.",
        ));
    }
    if let Some(mode) = value(5) {
        let text = match mode as i64 {
            0 => "none",
            1 => "full",
            2 => "incremental",
            _ => "unknown",
        };
        cards.push(card(
            "Auto vacuum",
            text,
            Level::Info,
            "Whether freed pages are given back by themselves.",
        ));
    }
    cards
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

    fn kv(pairs: &[(&str, &str)]) -> ResultSet {
        let rows: Vec<[Option<&str>; 2]> =
            pairs.iter().map(|(k, v)| [Some(*k), Some(*v)]).collect();
        let refs: Vec<&[Option<&str>]> = rows.iter().map(|r| r.as_slice()).collect();
        set(&["field", "value"], &refs)
    }

    fn card_of<'a>(cards: &'a [Card], label: &str) -> &'a Card {
        cards
            .iter()
            .find(|c| c.label == label)
            .unwrap_or_else(|| panic!("no {label} card"))
    }

    #[test]
    fn each_database_has_its_sections() {
        let ids = |server| sections(server).iter().map(|s| s.id).collect::<Vec<_>>();
        assert_eq!(ids(Server::Postgres)[..2], ["overview", "sessions"]);
        assert!(ids(Server::Postgres).contains(&"statements"));
        assert!(ids(Server::MySql).contains(&"slow"));
        assert_eq!(
            ids(Server::Redis),
            ["overview", "commands", "slowlog", "clients", "latency"]
        );
        assert!(ids(Server::Mongo).contains(&"operations"));
        assert!(ids(Server::Sqlite).contains(&"tables"));
        assert!(ids(Server::Unknown).is_empty());
        // Postgres tries older column names when the new ones fail.
        let postgres = sections(Server::Postgres);
        let statements = postgres
            .iter()
            .find(|s| s.id == "statements")
            .expect("statements");
        assert!(statements.sql.contains("total_exec_time"));
        assert!(
            statements
                .fallback
                .as_deref()
                .is_some_and(|sql| sql.contains("total_time"))
        );
        // What a database lacks is said.
        assert!(!Server::Sqlite.missing().is_empty());
        assert!(
            Server::Redis
                .missing()
                .iter()
                .any(|(what, _)| *what == "Locks")
        );
    }

    #[test]
    fn stops_sessions_per_database() {
        assert_eq!(
            Server::Postgres.stop("42", false).as_deref(),
            Some("SELECT pg_cancel_backend(42)")
        );
        assert_eq!(
            Server::Postgres.stop("42", true).as_deref(),
            Some("SELECT pg_terminate_backend(42)")
        );
        assert_eq!(
            Server::MySql.stop("7", false).as_deref(),
            Some("KILL QUERY 7")
        );
        assert_eq!(Server::MySql.stop("7", true).as_deref(), Some("KILL 7"));
        assert_eq!(
            Server::Redis.stop("3", true).as_deref(),
            Some("CLIENT KILL ID 3")
        );
        assert_eq!(Server::Redis.stop("3", false), None);
        assert_eq!(
            Server::Mongo.stop("123", false).as_deref(),
            Some("db.killOp(123)")
        );
        assert_eq!(
            Server::Mongo.stop("shard1:55", false).as_deref(),
            Some("db.killOp(\"shard1:55\")")
        );
        assert_eq!(Server::Sqlite.stop("1", true), None);
        // Nothing but a plain id reaches the server.
        assert_eq!(Server::Postgres.stop("1); DROP TABLE t; --", false), None);
        assert_eq!(Server::Mongo.stop("1\"); x", false), None);
        assert_eq!(Server::Postgres.stop("", false), None);
        assert!(Server::Postgres.cancels_query() && !Server::Redis.cancels_query());
    }

    #[test]
    fn reads_the_postgres_overview() {
        let row = set(
            &[
                "numbackends",
                "max_connections",
                "xact_commit",
                "xact_rollback",
                "blks_read",
                "blks_hit",
                "deadlocks",
                "temp_files",
                "temp_bytes",
                "size_bytes",
                "uptime_seconds",
            ],
            &[&[
                Some("95"),
                Some("100"),
                Some("900"),
                Some("100"),
                Some("50"),
                Some("950"),
                Some("2"),
                Some("0"),
                Some("0"),
                Some("1073741824"),
                Some("90000"),
            ]],
        );
        let cards = overview(Server::Postgres, &[row]);
        assert_eq!(card_of(&cards, "Connections").value, "95 / 100");
        assert_eq!(card_of(&cards, "Connections").level, Level::Bad);
        assert_eq!(card_of(&cards, "Cache hit ratio").value, "95.0%");
        assert_eq!(card_of(&cards, "Cache hit ratio").level, Level::Warn);
        assert_eq!(card_of(&cards, "Size").value, "1.0 GiB");
        assert_eq!(card_of(&cards, "Uptime").value, "1 d 1 h");
        assert_eq!(card_of(&cards, "Rollbacks").value, "10.0%");
        assert_eq!(card_of(&cards, "Deadlocks").level, Level::Warn);
        assert_eq!(card_of(&cards, "Temp files").level, Level::Good);
    }

    #[test]
    fn reads_the_mysql_overview_from_both_sets() {
        let status = kv(&[
            ("Threads_connected", "10"),
            ("Innodb_buffer_pool_read_requests", "1000000"),
            ("Innodb_buffer_pool_reads", "5000"),
            ("Questions", "6000"),
            ("Uptime", "600"),
            ("Slow_queries", "0"),
            ("Created_tmp_tables", "100"),
            ("Created_tmp_disk_tables", "50"),
        ]);
        let variables = kv(&[
            ("max_connections", "151"),
            ("slow_query_log", "ON"),
            ("long_query_time", "10.000000"),
        ]);
        let cards = overview(Server::MySql, &[status, variables]);
        assert_eq!(card_of(&cards, "Connections").value, "10 / 151");
        assert_eq!(card_of(&cards, "Buffer pool hit ratio").value, "99.5%");
        assert_eq!(card_of(&cards, "Buffer pool hit ratio").level, Level::Good);
        assert_eq!(card_of(&cards, "Queries per second").value, "10.0");
        assert_eq!(card_of(&cards, "Slow queries").level, Level::Good);
        assert_eq!(card_of(&cards, "Temp tables on disk").level, Level::Warn);
        assert_eq!(card_of(&cards, "Slow log").value, "on, over 10.000000 s");
    }

    #[test]
    fn reads_redis_info_and_commandstats() {
        let info = kv(&[
            ("used_memory", "104857600"),
            ("maxmemory", "104857600"),
            ("mem_fragmentation_ratio", "1.10"),
            ("keyspace_hits", "900"),
            ("keyspace_misses", "100"),
            ("instantaneous_ops_per_sec", "1200"),
            ("connected_clients", "5"),
            ("blocked_clients", "0"),
            ("evicted_keys", "7"),
            ("uptime_in_seconds", "172800"),
            ("role", "master"),
        ]);
        let cards = overview(Server::Redis, std::slice::from_ref(&info));
        assert_eq!(card_of(&cards, "Memory").value, "100.0 MiB / 100.0 MiB");
        assert_eq!(card_of(&cards, "Memory").level, Level::Bad);
        assert_eq!(card_of(&cards, "Hit ratio").value, "90.0%");
        assert_eq!(card_of(&cards, "Operations per second").value, "1200");
        assert_eq!(card_of(&cards, "Evicted keys").level, Level::Warn);
        assert_eq!(card_of(&cards, "Uptime").value, "2 d 0 h");
        let stats = kv(&[
            (
                "cmdstat_get",
                "calls=10,usec=100,usec_per_call=10.00,rejected_calls=0,failed_calls=1",
            ),
            (
                "cmdstat_zrange",
                "calls=2,usec=400,usec_per_call=200.00,rejected_calls=0,failed_calls=0",
            ),
        ]);
        let commands = redis_commands(&[stats]);
        // The slowest on average first.
        assert_eq!(commands.rows[0][0], "zrange");
        assert_eq!(commands.cell(1, "calls"), Some("10"));
        assert_eq!(commands.cell(1, "failed"), Some("1"));
    }

    #[test]
    fn reads_redis_slowlog_clients_and_latency() {
        let slow = set(
            &["value"],
            &[&[Some(
                r#"[[14, 1700000000, 25000, ["KEYS", "*"], "127.0.0.1:5000", ""], [13, 1699999999, 12000, ["SORT", "k"], "127.0.0.1:5001", "app"]]"#,
            )]],
        );
        let table = redis_slowlog(&[slow]);
        assert_eq!(table.rows.len(), 2);
        assert_eq!(table.cell(0, "command"), Some("KEYS *"));
        assert_eq!(table.cell(0, "duration_ms"), Some("25.00"));
        let clients = set(
            &["value"],
            &[&[Some(
                "id=3 addr=127.0.0.1:5000 name= age=10 idle=2 flags=N db=0 cmd=get\nid=4 addr=10.0.0.1:6000 name=app age=99 idle=0 flags=N db=1 cmd=client|list\n",
            )]],
        );
        let table = redis_clients(&[clients]);
        assert_eq!(table.rows.len(), 2);
        assert_eq!(table.cell(1, "id"), Some("4"));
        assert_eq!(table.cell(1, "name"), Some("app"));
        assert_eq!(table.cell(0, "name"), Some(""));
        let latency = set(
            &["value"],
            &[&[Some(r#"[["command", 1700000000, 12, 40]]"#)]],
        );
        let table = redis_latency(&[latency]);
        assert_eq!(table.cell(0, "max_ms"), Some("40"));
        // An empty answer is an empty table, not an error.
        assert!(
            redis_slowlog(&[set(&["value"], &[&[Some("[]")]])])
                .rows
                .is_empty()
        );
    }

    #[test]
    fn reads_mongo_status_operations_and_profile() {
        let status = set(
            &["document"],
            &[&[Some(
                r#"{"uptime": 3600, "connections": {"current": 20, "available": 80},
                    "mem": {"resident": 512},
                    "opcounters": {"insert": 1, "query": 5, "update": 2, "delete": 0, "command": 9},
                    "globalLock": {"currentQueue": {"total": 3}},
                    "wiredTiger": {"cache": {"bytes currently in the cache": 900, "maximum bytes configured": 1000}}}"#,
            )]],
        );
        let cards = overview(Server::Mongo, &[status]);
        assert_eq!(card_of(&cards, "Connections").value, "20 / 100");
        assert_eq!(card_of(&cards, "WiredTiger cache").level, Level::Bad);
        assert_eq!(card_of(&cards, "Resident memory").value, "512.0 MiB");
        assert!(card_of(&cards, "Operations").value.contains("query 5"));
        assert_eq!(card_of(&cards, "Queued by locks").level, Level::Warn);
        let ops = set(
            &["document"],
            &[&[Some(
                r#"{"inprog": [{"opid": 1234, "op": "query", "ns": "shop.orders", "secs_running": 12, "client": "10.0.0.1:1", "command": {"find": "orders"}}]}"#,
            )]],
        );
        let table = mongo_operations(&[ops]);
        assert_eq!(table.cell(0, "opid"), Some("1234"));
        assert_eq!(table.cell(0, "ns"), Some("shop.orders"));
        assert!(table.cell(0, "command").is_some_and(|c| c.contains("find")));
        let profile = set(
            &["document"],
            &[&[Some(
                r#"{"ts": "2026-10-26", "op": "query", "ns": "shop.orders", "millis": 250, "docsExamined": 5000, "nreturned": 2, "planSummary": "COLLSCAN"}"#,
            )]],
        );
        let table = mongo_profile(&[profile]);
        assert_eq!(table.cell(0, "plan"), Some("COLLSCAN"));
        assert_eq!(table.cell(0, "docs_examined"), Some("5000"));
    }

    #[test]
    fn reads_the_sqlite_pragmas() {
        let one = |text: &str| set(&["v"], &[&[Some(text)]]);
        let sets = [
            one("1000"),
            one("4096"),
            one("300"),
            one("wal"),
            one("-2000"),
            one("0"),
        ];
        let cards = overview(Server::Sqlite, &sets);
        assert_eq!(card_of(&cards, "File size").value, "3.9 MiB");
        assert_eq!(card_of(&cards, "Free pages").level, Level::Warn);
        assert_eq!(card_of(&cards, "Journal mode").level, Level::Good);
        assert_eq!(card_of(&cards, "Cache size").value, "2000 KiB");
        assert_eq!(card_of(&cards, "Auto vacuum").value, "none");
    }

    #[test]
    fn formats_sizes_and_times() {
        assert_eq!(bytes(512.0), "512 B");
        assert_eq!(bytes(1536.0), "1.5 KiB");
        assert_eq!(uptime(45.0), "45 s");
        assert_eq!(uptime(7200.0 + 120.0), "2 h 2 min");
        assert_eq!(Server::of(Language::Sql, Family::Other), Server::Unknown);
        assert_eq!(Server::of(Language::Redis, Family::Other), Server::Redis);
    }

    #[test]
    fn a_result_set_is_a_table() {
        let t = table(&set(&["a", "b"], &[&[Some("1"), None]]));
        assert_eq!(t.cell(0, "A"), Some("1"));
        assert_eq!(t.cell(0, "b"), Some(""));
        assert_eq!(t.cell(1, "a"), None);
    }
}
