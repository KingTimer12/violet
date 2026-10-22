//! Backup with `pg_dump`, restore with `pg_restore` (archives) or `psql`
//! (plain SQL), with the options DBeaver offers and a few more.

use violet_sdk::tools::{
    Command, Connection, Download, Field, Quoting, Target, Task, TaskKind, is, mapped, not, on,
    path, text, value,
};

/// Where PostgreSQL's client programs usually are, besides `PATH`.
const LOCATIONS: &[&str] = &[
    "/opt/homebrew/opt/libpq/bin",
    "/opt/homebrew/bin",
    "/usr/local/opt/libpq/bin",
    "/usr/local/bin",
    "/Applications/Postgres.app/Contents/Versions/*/bin",
    "/Library/PostgreSQL/*/bin",
    "/opt/homebrew/opt/postgresql@*/bin",
    "/usr/lib/postgresql/*/bin",
    "/usr/pgsql-*/bin",
    "/usr/bin",
    r"C:\Program Files\PostgreSQL\*\bin",
];

const FORMATS: &[(&str, &str)] = &[
    ("custom", "Custom (pg_restore)"),
    ("directory", "Directory (parallel)"),
    ("tar", "Tar"),
    ("plain", "Plain SQL"),
];

const CONTENT: &[(&str, &str)] = &[
    ("all", "Schema and data"),
    ("schema", "Schema only"),
    ("data", "Data only"),
];

pub fn tasks() -> Vec<Task> {
    vec![backup(), restore()]
}

fn targets(task: Task) -> Task {
    task.target(Target::directory().when("format", &["directory"]))
        .target(Target::file("sql").when("format", &["plain"]))
        .target(Target::file("tar").when("format", &["tar"]))
        .target(Target::file("dump"))
}

fn backup() -> Task {
    let task = Task::new(TaskKind::Backup, &["pg_dump"])
        .locations(LOCATIONS)
        .with_objects()
        // Format
        .field(
            Field::choice("format", "Format", "Format", "custom", FORMATS).help(
                "Custom and directory archives restore selectively and in parallel with \
                 pg_restore; plain SQL runs in psql or any editor.",
            ),
        )
        .field(
            Field::number("compression", "Compression", "Format", 0..=9, 6)
                .help("0 stores the data uncompressed, 9 compresses most.")
                .when("format", &["custom", "directory"]),
        )
        .field(
            Field::number("jobs", "Parallel jobs", "Format", 1..=32, 1)
                .help("Tables dumped at once. Each one opens a connection.")
                .when("format", &["directory"]),
        )
        .field(
            Field::text("encoding", "Encoding", "Format")
                .help("The output's character set, such as UTF8. Empty for the database's."),
        )
        // Content
        .field(Field::choice(
            "content", "Include", "Content", "all", CONTENT,
        ))
        .field(
            Field::choice(
                "data-as",
                "Write data as",
                "Content",
                "copy",
                &[
                    ("copy", "COPY (fastest)"),
                    ("inserts", "INSERT statements"),
                    ("column-inserts", "INSERT with column names"),
                ],
            )
            .help("INSERT statements load into other databases, but restore much slower.")
            .when("content", &["all", "data"]),
        )
        .field(
            Field::number(
                "rows-per-insert",
                "Rows per INSERT",
                "Content",
                1..=10_000,
                1,
            )
            .help("Needs pg_dump 12 or later when more than 1.")
            .when("data-as", &["inserts", "column-inserts"]),
        )
        .field(
            Field::flag("on-conflict", "ON CONFLICT DO NOTHING", "Content", false)
                .help("Skips rows that already exist when the INSERTs run.")
                .when("data-as", &["inserts", "column-inserts"]),
        )
        .field(
            Field::flag("large-objects", "Large objects", "Content", false)
                .help("Includes large objects even when only some schemas or tables are picked."),
        )
        .field(
            Field::text("exclude-tables", "Skip tables", "Content")
                .help("Patterns, separated by commas, such as public.audit_*."),
        )
        .field(
            Field::text("exclude-data", "Skip data of tables", "Content")
                .help("Patterns whose structure is kept but not their rows, such as logs.*."),
        )
        // Ownership
        .field(
            Field::flag("no-owner", "Discard owners", "Ownership", false)
                .help("Leaves out ALTER ... OWNER, so objects belong to whoever restores."),
        )
        .field(
            Field::flag("no-privileges", "Discard privileges", "Ownership", false)
                .help("Leaves out GRANT and REVOKE."),
        )
        .field(
            Field::flag(
                "set-session-authorization",
                "Use SET SESSION AUTHORIZATION",
                "Ownership",
                false,
            )
            .help("Sets owners with SET SESSION AUTHORIZATION instead of ALTER OWNER."),
        )
        .field(Field::flag(
            "no-tablespaces",
            "Discard tablespaces",
            "Ownership",
            false,
        ))
        .field(Field::flag(
            "no-comments",
            "Discard comments",
            "Ownership",
            false,
        ))
        // Script
        .field(
            Field::flag("clean", "Drop objects first", "Script", false)
                .help("Adds DROP statements before each CREATE.")
                .when("format", &["plain"]),
        )
        .field(
            Field::flag("if-exists", "Use IF EXISTS when dropping", "Script", true)
                .help("Applies with Drop objects first.")
                .when("format", &["plain"]),
        )
        .field(
            Field::flag("create", "Create the database", "Script", false)
                .help("Starts with CREATE DATABASE and connects to it.")
                .when("format", &["plain"]),
        )
        .field(
            Field::flag("disable-triggers", "Disable triggers", "Script", false)
                .help("Turns triggers off while data loads. Needs a superuser to restore.")
                .when("content", &["data"]),
        );
    targets(advanced(task))
        .connection(
            Connection::env("PGHOST", "PGPORT", "PGUSER", "PGDATABASE").password("PGPASSWORD"),
        )
        .command(backup_command())
}

fn restore() -> Task {
    let task = Task::new(TaskKind::Restore, &["pg_restore", "psql"])
        .locations(LOCATIONS)
        .field(
            Field::choice("format", "Format", "Input", "custom", FORMATS)
                .help("Violet reads it from the file you pick."),
        )
        .field(
            Field::choice("content", "Restore", "Input", "all", CONTENT)
                .when("format", &["custom", "directory", "tar"]),
        )
        .field(
            Field::text("database", "Into database", "Input")
                .help("Empty for the connection's database."),
        )
        .field(
            Field::flag("clean", "Drop objects first", "Restore", false)
                .help("Drops each object before creating it again.")
                .when("format", &["custom", "directory", "tar"]),
        )
        .field(
            Field::flag("if-exists", "Use IF EXISTS when dropping", "Restore", true)
                .help("Applies with Drop objects first.")
                .when("format", &["custom", "directory", "tar"]),
        )
        .field(
            Field::flag("create", "Create the database", "Restore", false)
                .help(
                    "Creates the database the backup was taken from, connecting to the one \
                     above first.",
                )
                .when("format", &["custom", "directory", "tar"]),
        )
        .field(
            Field::flag("single-transaction", "Single transaction", "Restore", false)
                .help("All or nothing: any error rolls everything back."),
        )
        .field(
            Field::flag("exit-on-error", "Stop at the first error", "Restore", true)
                .help("Otherwise errors are reported and the restore carries on."),
        )
        .field(
            Field::number("jobs", "Parallel jobs", "Restore", 1..=32, 1)
                .help("Not with Single transaction. Each job opens a connection.")
                .when("format", &["custom", "directory"]),
        )
        .field(
            Field::flag("no-owner", "Discard owners", "Ownership", false)
                .help("Objects belong to the user restoring them.")
                .when("format", &["custom", "directory", "tar"]),
        )
        .field(
            Field::flag("no-privileges", "Discard privileges", "Ownership", false)
                .help("Skips GRANT and REVOKE.")
                .when("format", &["custom", "directory", "tar"]),
        )
        .field(
            Field::text("role", "Run as role", "Ownership")
                .help("SET ROLE before restoring.")
                .when("format", &["custom", "directory", "tar"]),
        )
        .field(
            Field::flag("disable-triggers", "Disable triggers", "Ownership", false)
                .help("While data loads. Needs a superuser.")
                .when("content", &["data"]),
        );
    // pg_restore writes a script instead of restoring without --dbname.
    let mut connection = Connection::env("PGHOST", "PGPORT", "PGUSER", "PGDATABASE")
        .password("PGPASSWORD")
        .database_field("database");
    connection.database = Some(violet_sdk::tools::Binding::Prefix("--dbname=".into()));
    targets(advanced(task))
        .connection(connection)
        .command(psql_command())
        .command(pg_restore_command())
}

/// Settings both tasks share.
fn advanced(task: Task) -> Task {
    task.field(
        Field::flag(
            "verify-tls",
            "Verify the server certificate",
            "Connection",
            true,
        )
        .help(
            "Like Violet, checks the certificate against the system's trusted roots \
                 (sslrootcert=system, client 16 or later). Only with TLS required.",
        ),
    )
    .field(
        Field::text("lock-wait-timeout", "Lock wait timeout", "Advanced")
            .help("Gives up instead of waiting longer for a table lock, such as 30s.")
            .when("format", &["custom", "directory", "tar", "plain"]),
    )
    .field(
        Field::flag("verbose", "Detailed log", "Advanced", true)
            .help("Reports each object as it is processed."),
    )
    .field(
        Field::text("extra", "Extra arguments", "Advanced")
            .help("Passed as typed, after the others. Quotes group words."),
    )
}

const FORMAT_LETTERS: &[(&str, &str)] = &[
    ("custom", "c"),
    ("directory", "d"),
    ("tar", "t"),
    ("plain", "p"),
];

/// TLS as libpq takes it: from the profile's mode, verifying the server
/// certificate against the system's roots when asked.
fn tls(command: Command) -> Command {
    let require = is("@ssl", &["require"]);
    command
        .env(&[is("@ssl", &["disable"])], "PGSSLMODE", [text("disable")])
        .env(&[is("@ssl", &["prefer"])], "PGSSLMODE", [text("prefer")])
        .env(
            &[require.clone(), on("verify-tls")],
            "PGSSLMODE",
            [text("verify-full")],
        )
        .env(
            &[require.clone(), on("verify-tls")],
            "PGSSLROOTCERT",
            [text("system")],
        )
        .env(
            &[require, is("verify-tls", &["false"])],
            "PGSSLMODE",
            [text("require")],
        )
        .env(&[], "PGAPPNAME", [text("Violet")])
        .env(&[], "PGCONNECT_TIMEOUT", [text("15")])
}

/// The log, the lock timeout and what the user typed, shared by every
/// command.
fn tail(command: Command) -> Command {
    command
        .flag("verbose", true, "--verbose")
        .arg_when(
            &[not("lock-wait-timeout", &[""])],
            [text("--lock-wait-timeout="), value("lock-wait-timeout")],
        )
        .extra("extra")
}

fn backup_command() -> Command {
    let data = not("content", &["schema"]);
    let inserts = is("data-as", &["inserts", "column-inserts"]);
    let command = Command::new("pg_dump")
        // Never wait for a password prompt nobody can answer.
        .arg([text("--no-password")])
        .arg([text("--format="), mapped("format", FORMAT_LETTERS)])
        .arg([text("--file="), path()])
        .arg_when(
            &[is("format", &["custom", "directory"])],
            [text("--compress="), value("compression")],
        )
        .arg_when(
            &[is("format", &["directory"]), not("jobs", &["1"])],
            [text("--jobs="), value("jobs")],
        )
        .arg_when(
            &[not("encoding", &[""])],
            [text("--encoding="), value("encoding")],
        )
        .arg_when(&[is("content", &["schema"])], [text("--schema-only")])
        .arg_when(&[is("content", &["data"])], [text("--data-only")])
        .arg_when(
            &[data.clone(), is("data-as", &["inserts"])],
            [text("--inserts")],
        )
        .arg_when(
            &[data.clone(), is("data-as", &["column-inserts"])],
            [text("--column-inserts")],
        )
        .arg_when(
            &[
                data.clone(),
                inserts.clone(),
                not("rows-per-insert", &["1"]),
            ],
            [text("--rows-per-insert="), value("rows-per-insert")],
        )
        .arg_when(
            &[data.clone(), inserts, on("on-conflict")],
            [text("--on-conflict-do-nothing")],
        )
        .flag("large-objects", true, "--blobs")
        .each(
            &[],
            "exclude-tables",
            [text("--exclude-table="), value("exclude-tables")],
        )
        .each(
            &[data],
            "exclude-data",
            [text("--exclude-table-data="), value("exclude-data")],
        )
        .flag("no-owner", true, "--no-owner")
        .flag("no-privileges", true, "--no-privileges")
        .flag(
            "set-session-authorization",
            true,
            "--use-set-session-authorization",
        )
        .flag("no-tablespaces", true, "--no-tablespaces")
        .flag("no-comments", true, "--no-comments")
        .arg_when(&[is("format", &["plain"]), on("clean")], [text("--clean")])
        .arg_when(
            &[is("format", &["plain"]), on("clean"), on("if-exists")],
            [text("--if-exists")],
        )
        .arg_when(
            &[is("format", &["plain"]), on("create")],
            [text("--create")],
        )
        .arg_when(
            &[is("content", &["data"]), on("disable-triggers")],
            [text("--disable-triggers")],
        )
        // pg_dump ignores --schema once --table is given.
        .objects("--schema=", "--table=", Quoting::Double, Some(".*"));
    tls(tail(command))
}

/// Plain SQL runs in psql, without ~/.psqlrc. pg_dump 17 and later write
/// `SET transaction_timeout`, which servers before 17 reject, so Violet
/// swaps it for a call that only sets it where the server knows it.
fn psql_command() -> Command {
    let command = Command::new("psql")
        .when(is("format", &["plain"]))
        .arg([text("--no-password")])
        .arg([text("--no-psqlrc")])
        .flag("exit-on-error", true, "--set=ON_ERROR_STOP=1")
        .flag("single-transaction", true, "--single-transaction")
        .flag("verbose", true, "--echo-errors")
        // --single-transaction needs --file, so standard input goes as `-`.
        .arg([text("--file=-")])
        .extra("extra")
        .stdin_from_path()
        .replace_line(
            "SET transaction_timeout = 0;",
            "SELECT pg_catalog.set_config('transaction_timeout', '0', false) \
             WHERE current_setting('server_version_num')::int >= 170000;",
        )
        // Dumps that pick schemas create public, which every database has.
        .replace_line("CREATE SCHEMA public;", "CREATE SCHEMA IF NOT EXISTS public;");
    tls(command)
}

fn pg_restore_command() -> Command {
    let command = Command::new("pg_restore")
        .when(not("format", &["plain"]))
        .arg([text("--no-password")])
        .arg([text("--format="), mapped("format", FORMAT_LETTERS)])
        .arg_when(&[is("content", &["schema"])], [text("--schema-only")])
        .arg_when(&[is("content", &["data"])], [text("--data-only")])
        .flag("clean", true, "--clean")
        .arg_when(&[on("clean"), on("if-exists")], [text("--if-exists")])
        .flag("create", true, "--create")
        .flag("single-transaction", true, "--single-transaction")
        .flag("exit-on-error", true, "--exit-on-error")
        .flag("no-owner", true, "--no-owner")
        .flag("no-privileges", true, "--no-privileges")
        .arg_when(
            &[is("content", &["data"]), on("disable-triggers")],
            [text("--disable-triggers")],
        )
        .arg_when(
            &[
                not("jobs", &["1"]),
                is("single-transaction", &["false"]),
                is("format", &["custom", "directory"]),
            ],
            [text("--jobs="), value("jobs")],
        )
        .arg_when(&[not("role", &[""])], [text("--role="), value("role")]);
    // The archive last, after `--`, so no name reads as an option.
    tls(tail(command)).arg([text("--")]).arg([path()])
}

/// PostgreSQL's client programs, from theseus-rs/postgresql-binaries:
/// relocatable builds of PostgreSQL 18.6 (whose pg_dump reads servers back
/// to 9.2), of which only pg_dump, pg_restore, psql and their libraries
/// are kept.
pub fn downloads() -> Vec<Download> {
    const BASE: &str = "https://github.com/theseus-rs/postgresql-binaries/releases/download/18.6.0";
    let unix = |platform: &str, triple: &str, sha256: &str, size: u64| Download {
        platform: platform.into(),
        url: format!("{BASE}/postgresql-18.6.0-{triple}.tar.gz"),
        sha256: sha256.into(),
        size,
        label: "PostgreSQL 18.6 client".into(),
        root: format!("postgresql-18.6.0-{triple}"),
        extract: vec![
            "bin/pg_dump".into(),
            "bin/pg_restore".into(),
            "bin/psql".into(),
            "lib/".into(),
        ],
        bin: "bin".into(),
    };
    vec![
        unix(
            "macos-aarch64",
            "aarch64-apple-darwin",
            "a257bcdb8aa3301a13d6a5bcec48f8c9517045b7cbae71e50788b2615539e95b",
            13_081_312,
        ),
        unix(
            "macos-x86_64",
            "x86_64-apple-darwin",
            "f8918fbe747e0d79bda58ad8f8afcf23ac384b847ff3af18856f0825ba54b031",
            13_435_995,
        ),
        unix(
            "linux-x86_64",
            "x86_64-unknown-linux-gnu",
            "bb3d09f876b2383e25a8c9ce09e32d185a03656a23d074f5195542b1b1b3ca61",
            12_184_736,
        ),
        unix(
            "linux-aarch64",
            "aarch64-unknown-linux-gnu",
            "5d2b5e8be9e96bfc3d7f22090057a836ec36d5d092542d8d3b93cc145cf4ee13",
            11_979_851,
        ),
        Download {
            // On Windows the libraries sit beside the programs.
            extract: vec!["bin/".into()],
            ..unix(
                "windows-x86_64",
                "x86_64-pc-windows-msvc",
                "7da44c2dbcda3b49688ea08ce8cf99cfe677adf565f53a9145bf9002c74db7d5",
                54_068_902,
            )
        },
    ]
}

/// The format of a backup, from its first bytes or a folder's files.
pub fn inspect(head: &[u8], entries: &[String], directory: bool) -> Vec<(String, String)> {
    let format = if directory {
        entries
            .iter()
            .any(|entry| entry == "toc.dat")
            .then_some("directory")
    } else if head.starts_with(b"PGDMP") {
        Some("custom")
    } else if head.get(257..262) == Some(b"ustar") {
        Some("tar")
    } else if looks_like_text(head) {
        Some("plain")
    } else {
        None
    };
    format
        .map(|format| vec![("format".to_owned(), format.to_owned())])
        .unwrap_or_default()
}

/// UTF-8 (perhaps cut mid-character at the end) without control
/// characters other than whitespace.
fn looks_like_text(head: &[u8]) -> bool {
    let utf8 = match std::str::from_utf8(head) {
        Ok(_) => true,
        Err(error) => error.error_len().is_none(),
    };
    !head.is_empty()
        && utf8
        && !head
            .iter()
            .any(|b| b.is_ascii_control() && !b.is_ascii_whitespace())
}

#[cfg(test)]
mod tests {
    use super::*;
    use violet_sdk::tools::{CommandPart, Piece};

    /// Every condition and value names a declared setting (Violet refuses
    /// the task otherwise; this catches it before release).
    #[test]
    fn templates_name_declared_settings() {
        for task in tasks() {
            let ids: Vec<&str> = task.fields.iter().map(|f| f.id.as_str()).collect();
            let known = |field: &str| field == "@ssl" || ids.contains(&field);
            for command in &task.commands {
                assert!(task.programs.contains(&command.program));
                for condition in command
                    .when
                    .iter()
                    .chain(command.variables.iter().flat_map(|v| &v.when))
                {
                    assert!(known(&condition.field), "{}", condition.field);
                }
                for part in &command.parts {
                    match part {
                        CommandPart::Argument(argument) => {
                            for condition in &argument.when {
                                assert!(known(&condition.field), "{}", condition.field);
                            }
                            for piece in &argument.pieces {
                                if let Piece::Value(value) = piece {
                                    assert!(known(&value.field), "{}", value.field);
                                }
                            }
                        }
                        CommandPart::Extra(field) => assert!(known(field)),
                        CommandPart::Objects(_) => {}
                    }
                }
            }
        }
        assert_eq!(downloads().len(), 5);
    }

    #[test]
    fn formats_are_read_from_the_file() {
        let format = |head: &[u8], entries: &[&str], dir: bool| {
            let entries: Vec<String> = entries.iter().map(|e| (*e).to_owned()).collect();
            inspect(head, &entries, dir).first().map(|(_, v)| v.clone())
        };
        assert_eq!(
            format(b"PGDMP\x01\x0e", &[], false).as_deref(),
            Some("custom")
        );
        let mut tar = vec![0u8; 512];
        tar[257..262].copy_from_slice(b"ustar");
        assert_eq!(format(&tar, &[], false).as_deref(), Some("tar"));
        assert_eq!(
            format(b"--\n-- PostgreSQL database dump\n", &[], false).as_deref(),
            Some("plain")
        );
        assert_eq!(
            format(b"", &["toc.dat", "3001.dat.gz"], true).as_deref(),
            Some("directory")
        );
        assert_eq!(format(b"", &["x"], true), None);
        assert_eq!(format(&[0x1f, 0x8b, 0x08, 0x00], &[], false), None);
    }
}
