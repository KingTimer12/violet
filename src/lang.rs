//! What the editor needs to know of each language a connection's statements
//! are written in: SQL (`crate::sql`) or Redis (`violet_lang::redis`).

use std::ops::Range;

use violet_core::{Language, ResultSet};
use violet_lang::redis;

use crate::sql;

/// What runs for the cursor at `cursor`.
pub fn statement_at(language: Language, text: &str, cursor: usize) -> Option<Range<usize>> {
    match language {
        Language::Redis => redis::statement_at(text, cursor),
        Language::Sql | Language::Mongodb => sql::statement_at(text, cursor),
    }
}

/// Whether `text` may carry a password.
pub fn sets_secret(language: Language, text: &str) -> bool {
    match language {
        Language::Redis => redis::sets_secret(text),
        Language::Sql | Language::Mongodb => sql::sets_secret(text),
    }
}

/// `text` as the history keeps it: passwords hidden.
pub fn redact_secrets(language: Language, text: &str) -> String {
    match language {
        Language::Redis => redis::redact_secrets(text),
        Language::Sql | Language::Mongodb => sql::redact_secrets(text),
    }
}

/// What in `text` the user confirms before it runs: SQL changing every
/// row of a table, Redis commands that flush, block or reconfigure the
/// server.
pub fn dangers(language: Language, text: &str) -> Vec<String> {
    match language {
        Language::Redis => redis::dangers(text),
        Language::Sql | Language::Mongodb => sql::unbounded_writes(text),
    }
}

/// What explains the statement `text` where the language has no plan view:
/// MongoDB's `explain()` around the query, and for Redis, which has no
/// plans, what the command's key weighs and how it is stored.
///
/// # Errors
///
/// Why `text` cannot be explained.
pub fn explain_script(language: Language, text: &str, analyze: bool) -> Result<String, String> {
    match language {
        Language::Mongodb => crate::mongo::explain(text, analyze),
        Language::Redis => redis_profile(text),
        Language::Sql => Err("SQL statements explain as plans".into()),
    }
}

/// The commands that profile the key of the Redis command `text`: its type,
/// the bytes it takes, how it is encoded and how long it has to live.
fn redis_profile(text: &str) -> Result<String, String> {
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#') && !line.starts_with("//"))
        .ok_or("Nothing to analyze")?;
    let args = redis::argv(line)?;
    let words: Vec<String> = args
        .iter()
        .map(|arg| String::from_utf8_lossy(arg).into_owned())
        .collect();
    let command = redis::command(&words).ok_or("Violet does not know this command")?;
    // A key comes first in a command's syntax, after its name.
    let name_words = command.name.split(' ').count();
    if !command.syntax.starts_with("key") || args.len() <= name_words {
        return Err(format!(
            "{} takes no key: Redis has no query plans, and Violet profiles a command's key",
            command.name
        ));
    }
    let key = redis::quote(&args[name_words]);
    Ok(format!(
        "TYPE {key}\nMEMORY USAGE {key}\nOBJECT ENCODING {key}\nPTTL {key}"
    ))
}

/// The cheapest statement of the language, to time a round trip with.
pub fn ping(language: Language) -> &'static str {
    match language {
        Language::Sql => "SELECT 1",
        Language::Redis => "PING",
        Language::Mongodb => "db.runCommand({ ping: 1 })",
    }
}

/// The first cell of a result set of one cell.
fn only_cell(set: &ResultSet) -> Option<&str> {
    set.rows.get(0, 0)?
}

/// Bytes as `1.5 KiB (1536 bytes)`.
fn human_bytes(bytes: u64) -> String {
    let mut size = bytes as f64;
    let mut unit = "B";
    for next in ["KiB", "MiB", "GiB"] {
        if size < 1024.0 {
            break;
        }
        size /= 1024.0;
        unit = next;
    }
    if unit == "B" {
        format!("{bytes} bytes")
    } else {
        format!("{size:.1} {unit} ({bytes} bytes)")
    }
}

/// What `PTTL` answered, in words.
fn human_ttl(pttl: i64) -> String {
    match pttl {
        -2 => "the key does not exist".to_owned(),
        -1 => "no expiry".to_owned(),
        ms if ms < 1000 => format!("{ms} ms"),
        ms if ms < 60_000 => format!("{:.1} s", ms as f64 / 1000.0),
        ms if ms < 3_600_000 => format!("{:.1} min", ms as f64 / 60_000.0),
        ms => format!("{:.1} h", ms as f64 / 3_600_000.0),
    }
}

/// The four answers of [`explain_script`] for Redis as one table of what
/// the key is; `None` for any other result.
pub fn redis_profile_report(sets: &[ResultSet]) -> Option<ResultSet> {
    let [kind, memory, encoding, ttl] = sets else {
        return None;
    };
    let kind = only_cell(kind)?;
    let mut rows = violet_core::Rows::new(2);
    rows.push([Some("Type"), Some(kind)]);
    if kind == "none" {
        rows.push([Some("Key"), Some("does not exist")]);
    } else {
        let memory = only_cell(memory)
            .and_then(|bytes| bytes.parse().ok())
            .map_or_else(|| "unknown".to_owned(), human_bytes);
        rows.push([Some("Memory"), Some(&memory)]);
        rows.push([Some("Encoding"), only_cell(encoding)]);
        let ttl = only_cell(ttl)
            .and_then(|ms| ms.parse().ok())
            .map_or_else(|| "unknown".to_owned(), human_ttl);
        rows.push([Some("Time to live"), Some(&ttl)]);
    }
    Some(ResultSet {
        columns: ["Property", "Value"]
            .map(|name| violet_core::ColumnMeta {
                name: name.to_owned(),
                type_name: None,
                numeric: false,
            })
            .to_vec(),
        rows,
        affected: None,
        command: None,
        truncated: false,
    })
}

/// The extension scripts are saved with, without the dot.
pub fn extension(language: Language) -> &'static str {
    match language {
        Language::Redis => "redis",
        Language::Sql | Language::Mongodb => "sql",
    }
}

/// What an editor of the language is called in menus.
pub fn editor_name(language: Language) -> &'static str {
    match language {
        Language::Redis => "Redis editor",
        Language::Sql | Language::Mongodb => "SQL editor",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_language_pings() {
        assert_eq!(ping(Language::Sql), "SELECT 1");
        assert_eq!(ping(Language::Redis), "PING");
        assert!(ping(Language::Mongodb).contains("ping"));
    }

    #[test]
    fn explains_by_language() {
        assert_eq!(
            explain_script(Language::Mongodb, "db.t.find()", false).as_deref(),
            Ok("db.t.find().explain(\"queryPlanner\")")
        );
        assert_eq!(
            explain_script(Language::Redis, "# note\nHGETALL user:1", false).as_deref(),
            Ok("TYPE user:1\nMEMORY USAGE user:1\nOBJECT ENCODING user:1\nPTTL user:1")
        );
        // The key is quoted as Redis reads it.
        assert!(
            explain_script(Language::Redis, "GET \"a b\"", false)
                .is_ok_and(|script| script.starts_with("TYPE \"a b\""))
        );
        assert!(explain_script(Language::Redis, "PING", false).is_err());
        assert!(explain_script(Language::Redis, "GET", false).is_err());
        assert!(explain_script(Language::Redis, "NOPE x", false).is_err());
        assert!(explain_script(Language::Sql, "SELECT 1", false).is_err());
    }

    fn cell_set(text: Option<&str>) -> ResultSet {
        let mut rows = violet_core::Rows::new(1);
        rows.push([text]);
        ResultSet {
            columns: vec![violet_core::ColumnMeta {
                name: "value".into(),
                type_name: None,
                numeric: false,
            }],
            rows,
            ..ResultSet::default()
        }
    }

    #[test]
    fn a_key_profile_reads_as_one_table() {
        let sets = [
            cell_set(Some("hash")),
            cell_set(Some("2048")),
            cell_set(Some("listpack")),
            cell_set(Some("90000")),
        ];
        let report = redis_profile_report(&sets).expect("a report");
        let rows: Vec<Vec<Option<String>>> = report.rows.iter().map(|row| row.to_vec()).collect();
        assert_eq!(rows[0], [Some("Type".into()), Some("hash".into())]);
        assert_eq!(rows[1][1].as_deref(), Some("2.0 KiB (2048 bytes)"));
        assert_eq!(rows[2][1].as_deref(), Some("listpack"));
        assert_eq!(rows[3][1].as_deref(), Some("1.5 min"));
        let missing = [
            cell_set(Some("none")),
            cell_set(None),
            cell_set(None),
            cell_set(Some("-2")),
        ];
        let report = redis_profile_report(&missing).expect("a report");
        assert_eq!(report.rows.len(), 2);
        assert!(redis_profile_report(&sets[..2]).is_none());
        assert_eq!(human_ttl(-1), "no expiry");
        assert_eq!(human_bytes(12), "12 bytes");
    }

    #[test]
    fn each_language_keeps_its_own_secrets() {
        assert_eq!(redact_secrets(Language::Redis, "AUTH hunter2"), "AUTH •••");
        assert!(!sets_secret(Language::Sql, "AUTH hunter2"));
        assert!(sets_secret(Language::Sql, "ALTER ROLE r PASSWORD 'x'"));
        assert_eq!(dangers(Language::Redis, "FLUSHDB").len(), 1);
        assert_eq!(dangers(Language::Sql, "FLUSHDB").len(), 0);
        assert_eq!(extension(Language::Redis), "redis");
        assert_eq!(
            statement_at(
                Language::Redis,
                "GET a
GET b",
                7
            ),
            Some(6..11)
        );
    }
}
