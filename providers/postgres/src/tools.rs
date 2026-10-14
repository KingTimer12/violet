//! Backup with `pg_dump`, restore with `pg_restore` (archives) or `psql`
//! (plain SQL), with the options DBeaver offers and a few more.

use violet_sdk::tools::{
    Field, ObjectRef, Request, Target, Task, TaskKind, ToolCommand, split_args,
};
use violet_sdk::{ConnectParams, DbError, Result, SslMode};

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
    targets(advanced(task))
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

pub fn command(kind: TaskKind, request: Request) -> Result<ToolCommand> {
    match kind {
        TaskKind::Backup => backup_command(&request),
        TaskKind::Restore => restore_command(&request),
    }
}

fn backup_command(request: &Request) -> Result<ToolCommand> {
    let format = request.setting("format");
    let letter = match format {
        "custom" => "c",
        "directory" => "d",
        "tar" => "t",
        "plain" => "p",
        other => return Err(DbError::unsupported(format!("Unknown format {other}"))),
    };
    let content = request.setting("content");
    let data = content != "schema";
    let inserts = data && request.setting("data-as") != "copy";
    let plain = format == "plain";

    let mut command = connection(ToolCommand::new("pg_dump"), request, None)
        .arg(format!("--format={letter}"))
        .arg(format!("--file={}", request.path));
    if matches!(format, "custom" | "directory") {
        let level = request.number("compression").unwrap_or(6);
        command = command.arg(format!("--compress={level}"));
    }
    if format == "directory" {
        let jobs = request.number("jobs").unwrap_or(1);
        command = command.arg_if(jobs > 1, format!("--jobs={jobs}"));
    }
    let encoding = request.setting("encoding").trim();
    command = command.arg_if(!encoding.is_empty(), format!("--encoding={encoding}"));
    command = match content {
        "schema" => command.arg("--schema-only"),
        "data" => command.arg("--data-only"),
        _ => command,
    };
    if inserts {
        command = command.arg(if request.setting("data-as") == "column-inserts" {
            "--column-inserts"
        } else {
            "--inserts"
        });
        let rows = request.number("rows-per-insert").unwrap_or(1);
        command = command
            .arg_if(rows > 1, format!("--rows-per-insert={rows}"))
            .arg_if(request.flag("on-conflict"), "--on-conflict-do-nothing");
    }
    command = command.arg_if(request.flag("large-objects"), "--blobs");
    for pattern in patterns(request.setting("exclude-tables")) {
        command = command.arg(format!("--exclude-table={pattern}"));
    }
    if data {
        for pattern in patterns(request.setting("exclude-data")) {
            command = command.arg(format!("--exclude-table-data={pattern}"));
        }
    }
    command = command
        .arg_if(request.flag("no-owner"), "--no-owner")
        .arg_if(request.flag("no-privileges"), "--no-privileges")
        .arg_if(
            request.flag("set-session-authorization"),
            "--use-set-session-authorization",
        )
        .arg_if(request.flag("no-tablespaces"), "--no-tablespaces")
        .arg_if(request.flag("no-comments"), "--no-comments");
    if plain {
        let clean = request.flag("clean");
        command = command
            .arg_if(clean, "--clean")
            .arg_if(clean && request.flag("if-exists"), "--if-exists")
            .arg_if(request.flag("create"), "--create");
    }
    command = command.arg_if(
        content == "data" && request.flag("disable-triggers"),
        "--disable-triggers",
    );
    for object in &request.objects {
        command = command.arg(object_arg(object));
    }
    Ok(tail(command, request))
}

fn restore_command(request: &Request) -> Result<ToolCommand> {
    let format = request.setting("format");
    let database = request.setting("database").trim();
    let database = (!database.is_empty()).then_some(database);
    let single = request.flag("single-transaction");
    let exit_on_error = request.flag("exit-on-error");

    if format == "plain" {
        let command = connection(ToolCommand::new("psql"), request, database)
            // No ~/.psqlrc: only the file runs.
            .arg("--no-psqlrc")
            .arg_if(exit_on_error, "--set=ON_ERROR_STOP=1")
            .arg_if(single, "--single-transaction")
            .arg_if(request.flag("verbose"), "--echo-errors")
            .arg(format!("--file={}", request.path));
        return Ok(extra(command, request));
    }
    let letter = match format {
        "custom" => "c",
        "directory" => "d",
        "tar" => "t",
        other => return Err(DbError::unsupported(format!("Unknown format {other}"))),
    };
    let mut command = connection(ToolCommand::new("pg_restore"), request, database)
        .arg(format!("--format={letter}"));
    command = match request.setting("content") {
        "schema" => command.arg("--schema-only"),
        "data" => command.arg("--data-only"),
        _ => command,
    };
    let clean = request.flag("clean");
    command = command
        .arg_if(clean, "--clean")
        .arg_if(clean && request.flag("if-exists"), "--if-exists")
        .arg_if(request.flag("create"), "--create")
        .arg_if(single, "--single-transaction")
        .arg_if(exit_on_error, "--exit-on-error")
        .arg_if(request.flag("no-owner"), "--no-owner")
        .arg_if(request.flag("no-privileges"), "--no-privileges")
        .arg_if(
            request.setting("content") == "data" && request.flag("disable-triggers"),
            "--disable-triggers",
        );
    let jobs = request.number("jobs").unwrap_or(1);
    command = command.arg_if(
        jobs > 1 && !single && format != "tar",
        format!("--jobs={jobs}"),
    );
    let role = request.setting("role").trim();
    command = command.arg_if(!role.is_empty(), format!("--role={role}"));
    let mut command = tail(command, request);
    // The archive goes last, after `--` so no name reads as an option.
    command = command.arg("--").arg(request.path.clone());
    Ok(command)
}

/// The connection, as one conninfo string: host, port, user, database and
/// TLS, with the password left to `PGPASSWORD`.
fn connection(command: ToolCommand, request: &Request, database: Option<&str>) -> ToolCommand {
    let params = &request.params;
    let mut info = Vec::new();
    if !params.host.trim().is_empty() {
        info.push(format!("host={}", quote(params.host.trim())));
    }
    info.push(format!("port={}", params.port));
    if !params.username.is_empty() {
        info.push(format!("user={}", quote(&params.username)));
    }
    let database = database.unwrap_or(&params.database);
    if !database.is_empty() {
        info.push(format!("dbname={}", quote(database)));
    }
    info.push(format!(
        "sslmode={}",
        sslmode(params, request.flag("verify-tls"))
    ));
    if params.ssl == SslMode::Require && request.flag("verify-tls") {
        info.push("sslrootcert=system".to_owned());
    }
    info.push("application_name=Violet".to_owned());
    info.push("connect_timeout=15".to_owned());
    command
        .arg(format!("--dbname={}", info.join(" ")))
        // Never wait for a password prompt nobody can answer.
        .arg("--no-password")
        .password_env("PGPASSWORD")
}

fn sslmode(params: &ConnectParams, verify: bool) -> &'static str {
    match params.ssl {
        SslMode::Disable => "disable",
        SslMode::Prefer => "prefer",
        SslMode::Require if verify => "verify-full",
        SslMode::Require => "require",
    }
}

/// Shared ending: the log, the lock timeout and what the user typed.
fn tail(command: ToolCommand, request: &Request) -> ToolCommand {
    let timeout = request.setting("lock-wait-timeout").trim();
    let command = command.arg_if(request.flag("verbose"), "--verbose").arg_if(
        !timeout.is_empty(),
        format!("--lock-wait-timeout={timeout}"),
    );
    extra(command, request)
}

fn extra(mut command: ToolCommand, request: &Request) -> ToolCommand {
    for arg in split_args(request.setting("extra")) {
        command = command.arg(arg);
    }
    command
}

/// A conninfo value, single-quoted with `\` and `'` escaped.
fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\\', r"\\").replace('\'', r"\'"))
}

/// A pg_dump name pattern matching `name` exactly: double-quoted, so case
/// and wildcard characters are taken literally.
fn exact(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn object_arg(object: &ObjectRef) -> String {
    match &object.name {
        Some(name) => format!("--table={}.{}", exact(&object.schema), exact(name)),
        None => format!("--schema={}", exact(&object.schema)),
    }
}

fn patterns(text: &str) -> impl Iterator<Item = &str> {
    text.split(',').map(str::trim).filter(|p| !p.is_empty())
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

    fn request(settings: &[(&str, &str)], objects: Vec<ObjectRef>) -> Request {
        let task = backup();
        let mut all: Vec<(String, String)> = task
            .fields
            .iter()
            .map(|f| (f.id.clone(), f.default.clone()))
            .collect();
        for (k, v) in settings {
            if let Some(slot) = all.iter_mut().find(|(id, _)| id == k) {
                slot.1 = (*v).to_owned();
            } else {
                all.push(((*k).to_owned(), (*v).to_owned()));
            }
        }
        Request {
            params: ConnectParams {
                host: "db.example.com".into(),
                port: 5432,
                database: "shop".into(),
                username: "o'neil".into(),
                password: None,
                ssl: SslMode::Require,
                options: Vec::new(),
            },
            settings: all,
            objects,
            path: "/backups/shop.dump".into(),
        }
    }

    #[test]
    fn backups_pass_the_connection_without_the_password() {
        let command = backup_command(&request(&[], Vec::new())).expect("command");
        assert_eq!(command.program, "pg_dump");
        assert_eq!(command.password_env.as_deref(), Some("PGPASSWORD"));
        assert_eq!(
            command.args[0],
            "--dbname=host='db.example.com' port=5432 user='o\\'neil' dbname='shop' \
             sslmode=verify-full sslrootcert=system application_name=Violet connect_timeout=15"
        );
        assert!(command.args.contains(&"--no-password".to_owned()));
        assert!(command.args.contains(&"--format=c".to_owned()));
        assert!(command.args.contains(&"--compress=6".to_owned()));
        assert!(
            command
                .args
                .contains(&"--file=/backups/shop.dump".to_owned())
        );
        assert!(command.args.contains(&"--verbose".to_owned()));
        assert!(!command.args.iter().any(|a| a.contains("clean")));
    }

    #[test]
    fn backup_options_map_to_pg_dump_flags() {
        let objects = vec![
            ObjectRef {
                schema: "Sales".into(),
                name: None,
            },
            ObjectRef {
                schema: "public".into(),
                name: Some("odd\"name".into()),
            },
        ];
        let command = backup_command(&request(
            &[
                ("format", "plain"),
                ("content", "data"),
                ("data-as", "column-inserts"),
                ("rows-per-insert", "100"),
                ("clean", "true"),
                ("no-owner", "true"),
                ("disable-triggers", "true"),
                ("exclude-data", "logs.*, audit"),
                ("extra", "--lock-wait-timeout=5s '--exclude-schema=a b'"),
                ("verify-tls", "false"),
            ],
            objects,
        ))
        .expect("command");
        let args = &command.args;
        for expected in [
            "--format=p",
            "--data-only",
            "--column-inserts",
            "--rows-per-insert=100",
            "--clean",
            "--if-exists",
            "--no-owner",
            "--disable-triggers",
            "--exclude-table-data=logs.*",
            "--exclude-table-data=audit",
            "--schema=\"Sales\"",
            "--table=\"public\".\"odd\"\"name\"",
            "--exclude-schema=a b",
        ] {
            assert!(
                args.contains(&expected.to_owned()),
                "{expected} in {args:?}"
            );
        }
        assert!(args[0].contains("sslmode=require "));
        assert!(!args.iter().any(|a| a.starts_with("--compress")));
    }

    #[test]
    fn restores_pick_the_program_by_format() {
        let mut request = request(&[("format", "plain")], Vec::new());
        let task = restore();
        request.settings = task
            .fields
            .iter()
            .map(|f| (f.id.clone(), f.default.clone()))
            .collect();
        request.settings.retain(|(k, _)| k != "format");
        request.settings.push(("format".into(), "plain".into()));
        let command = restore_command(&request).expect("command");
        assert_eq!(command.program, "psql");
        assert!(command.args.contains(&"--set=ON_ERROR_STOP=1".to_owned()));
        assert!(
            command
                .args
                .contains(&"--file=/backups/shop.dump".to_owned())
        );

        request
            .settings
            .retain(|(k, _)| !matches!(k.as_str(), "format" | "jobs" | "database"));
        request.settings.push(("format".into(), "directory".into()));
        request.settings.push(("jobs".into(), "4".into()));
        request.settings.push(("database".into(), "copy".into()));
        let command = restore_command(&request).expect("command");
        assert_eq!(command.program, "pg_restore");
        assert!(command.args[0].contains("dbname='copy'"));
        assert!(command.args.contains(&"--jobs=4".to_owned()));
        assert!(command.args.contains(&"--exit-on-error".to_owned()));
        let n = command.args.len();
        assert_eq!(command.args[n - 2..], ["--", "/backups/shop.dump"]);
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
