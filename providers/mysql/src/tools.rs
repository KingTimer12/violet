//! Backup with `mysqldump`, restore with `mysql`, both as SQL scripts.
//!
//! Options follow MySQL's client programs; a setting switches TLS options
//! to MariaDB's spelling for its clients, which name them differently.

use violet_sdk::tools::{
    Binding, Command, Connection, Download, Field, Piece, Target, Task, TaskKind, is, mapped, not,
    on, path, text, value,
};

/// Where MySQL's and MariaDB's client programs usually are, besides `PATH`.
const LOCATIONS: &[&str] = &[
    "/opt/homebrew/opt/mysql-client/bin",
    "/opt/homebrew/opt/mysql-client@*/bin",
    "/opt/homebrew/opt/mysql/bin",
    "/opt/homebrew/opt/mysql@*/bin",
    "/opt/homebrew/opt/mariadb/bin",
    "/opt/homebrew/bin",
    "/usr/local/opt/mysql-client/bin",
    "/usr/local/mysql/bin",
    "/usr/local/bin",
    "/usr/bin",
    r"C:\Program Files\MySQL\MySQL Server *\bin",
    r"C:\Program Files\MySQL\MySQL Shell *\bin",
    r"C:\Program Files\MariaDB *\bin",
];

const CONTENT: &[(&str, &str)] = &[
    ("all", "Schema and data"),
    ("schema", "Schema only"),
    ("data", "Data only"),
];

const CLIENTS: &[(&str, &str)] = &[("mysql", "MySQL"), ("mariadb", "MariaDB")];

pub fn tasks() -> Vec<Task> {
    vec![backup(), restore()]
}

/// The profile's host, port and user as options and the password through
/// `MYSQL_PWD`. `mysqldump` takes the database as a name after the
/// options (see [`dump_command`]); `mysql` as `--database`, left out when
/// there is none.
fn connection(database: Option<&str>) -> Connection {
    let option = |prefix: &str| Some(Binding::Prefix(prefix.into()));
    Connection {
        host: option("--host="),
        port: option("--port="),
        user: option("--user="),
        database: database.and_then(option),
        password_env: Some("MYSQL_PWD".into()),
        database_field: Some("database".into()),
    }
}

fn backup() -> Task {
    let task = Task::new(TaskKind::Backup, &["mysqldump"])
        .locations(LOCATIONS)
        // Source
        .field(
            Field::text("database", "Database", "Source")
                .help("Empty for the connection's database; one of them is needed."),
        )
        .field(
            Field::text("tables", "Only tables", "Source")
                .help("Names, separated by commas. Empty for every table.")
                .when("create-database", &["false"]),
        )
        .field(
            Field::text("exclude-tables", "Skip tables", "Source")
                .help("Names, separated by commas, left out entirely."),
        )
        // Content
        .field(Field::choice(
            "content", "Include", "Content", "all", CONTENT,
        ))
        .field(
            Field::flag("routines", "Procedures and functions", "Content", true)
                .when("content", &["all", "schema"]),
        )
        .field(
            Field::flag("triggers", "Triggers", "Content", true)
                .when("content", &["all", "schema"]),
        )
        .field(
            Field::flag("events", "Scheduled events", "Content", false)
                .help("Needs the EVENT privilege.")
                .when("content", &["all", "schema"]),
        )
        .field(
            Field::choice(
                "inserts",
                "Write data as",
                "Content",
                "extended",
                &[
                    ("extended", "INSERT with many rows (fastest)"),
                    ("complete", "INSERT with column names"),
                    ("single", "One INSERT per row"),
                ],
            )
            .help("Column names and single rows load into other databases more easily.")
            .when("content", &["all", "data"]),
        )
        .field(
            Field::choice(
                "conflicts",
                "Rows that already exist",
                "Content",
                "error",
                &[
                    ("error", "Fail"),
                    ("ignore", "Skip (INSERT IGNORE)"),
                    ("replace", "Replace (REPLACE)"),
                ],
            )
            .when("content", &["all", "data"]),
        )
        .field(
            Field::flag("hex-blob", "Binary columns in hex", "Content", true)
                .help("Writes binary strings and blobs as 0x... so the file stays plain text.")
                .when("content", &["all", "data"]),
        )
        // Script
        .field(
            Field::flag("create-database", "Create the database", "Script", false)
                .help("Starts with CREATE DATABASE and USE, so it restores under the same name."),
        )
        .field(
            Field::flag("drop-database", "Drop the database first", "Script", false)
                .when("create-database", &["true"]),
        )
        .field(
            Field::flag("drop-tables", "Drop tables first", "Script", true)
                .help("Adds DROP TABLE IF EXISTS before each CREATE TABLE.")
                .when("content", &["all", "schema"]),
        )
        .field(
            Field::text("encoding", "Encoding", "Script")
                .help("The script's character set. Empty for utf8mb4."),
        )
        // Consistency
        .field(
            Field::flag(
                "single-transaction",
                "Consistent snapshot",
                "Consistency",
                true,
            )
            .help(
                "Reads InnoDB tables in one transaction instead of locking them. \
                     Other engines may still change during the backup.",
            ),
        )
        .field(
            Field::flag("no-tablespaces", "Skip tablespaces", "Consistency", true)
                .help("Tablespace statements need the PROCESS privilege to dump."),
        )
        .field(
            Field::choice(
                "gtid",
                "GTID_PURGED",
                "Consistency",
                "auto",
                &[
                    ("auto", "When the server uses GTIDs"),
                    ("off", "Never"),
                    ("on", "Always"),
                    ("comment", "As a comment"),
                ],
            )
            .help("Off restores into servers without GTIDs or with others of their own.")
            .when("client", &["mysql"]),
        );
    advanced(task)
        .target(Target::file("sql"))
        .connection(connection(None))
        .command(dump_command())
}

fn restore() -> Task {
    let task = Task::new(TaskKind::Restore, &["mysql"])
        .locations(LOCATIONS)
        .field(
            Field::text("database", "Into database", "Restore")
                .help("Empty for the connection's database, or the one the script uses."),
        )
        .field(
            Field::flag("force", "Carry on after errors", "Restore", false)
                .help("Otherwise the restore stops at the first failing statement."),
        )
        .field(
            Field::flag(
                "sandbox",
                "Refuse shell commands in the file",
                "Restore",
                true,
            )
            .help(
                "The script may not run programs or read files on this machine. \
                     Needs mysql 8.0.40 or MariaDB 10.5.25 and later.",
            ),
        )
        .field(
            Field::text("encoding", "Encoding", "Restore")
                .help("The script's character set. Empty for utf8mb4."),
        );
    advanced(task)
        .target(Target::file("sql"))
        .connection(connection(Some("--database=")))
        .command(restore_command())
}

/// Settings both tasks share.
fn advanced(task: Task) -> Task {
    task.field(
        Field::choice("client", "Client programs", "Connection", "mysql", CLIENTS)
            .help("Whose programs run: their TLS options differ. Violet's download is MySQL's."),
    )
    .field(Field::text("ssl-ca", "CA certificates", "Connection").help(
        "A file of trusted certificates, such as /etc/ssl/cert.pem. When set, \
             the server certificate and host name are verified; otherwise the \
             connection is encrypted without checking whom it reaches. Only with \
             TLS required.",
    ))
    .field(
        Field::flag("verbose", "Detailed log", "Advanced", true)
            .help("Reports each step as it runs."),
    )
    .field(
        Field::text("extra", "Extra arguments", "Advanced")
            .help("Passed as typed, after the others. Quotes group words."),
    )
}

/// TCP, TLS as the profile asks, the character set, the log and what the
/// user typed: shared by both commands.
fn common(command: Command) -> Command {
    let mysql = is("client", &["mysql"]);
    let mariadb = is("client", &["mariadb"]);
    let require = is("@ssl", &["require"]);
    let ca = not("ssl-ca", &[""]);
    let no_ca = is("ssl-ca", &[""]);
    command
        .arg([text("--protocol=TCP")])
        // MySQL: one option, verifying only with a CA file.
        .arg_when(
            &[mysql.clone(), is("@ssl", &["disable"])],
            [text("--ssl-mode=DISABLED")],
        )
        .arg_when(
            &[mysql.clone(), is("@ssl", &["prefer"])],
            [text("--ssl-mode=PREFERRED")],
        )
        .arg_when(
            &[mysql.clone(), require.clone(), no_ca.clone()],
            [text("--ssl-mode=REQUIRED")],
        )
        .arg_when(
            &[mysql, require.clone(), ca.clone()],
            [text("--ssl-mode=VERIFY_IDENTITY")],
        )
        // MariaDB: TLS on or off, and verification on top.
        .arg_when(
            &[mariadb.clone(), is("@ssl", &["disable"])],
            [text("--skip-ssl")],
        )
        .arg_when(&[mariadb.clone(), require.clone()], [text("--ssl")])
        .arg_when(
            &[mariadb.clone(), require.clone(), no_ca],
            [text("--skip-ssl-verify-server-cert")],
        )
        .arg_when(
            &[mariadb, require.clone(), ca.clone()],
            [text("--ssl-verify-server-cert")],
        )
        .arg_when(&[require, ca], [text("--ssl-ca="), value("ssl-ca")])
        .arg_when(
            &[is("encoding", &[""])],
            [text("--default-character-set=utf8mb4")],
        )
        .arg_when(
            &[not("encoding", &[""])],
            [text("--default-character-set="), value("encoding")],
        )
        .flag("verbose", true, "--verbose")
}

fn dump_command() -> Command {
    let schema = not("content", &["data"]);
    let data = not("content", &["schema"]);
    let command = Command::new("mysqldump")
        .arg([text("--result-file="), path()])
        .arg_when(&[is("content", &["schema"])], [text("--no-data")])
        .arg_when(&[is("content", &["data"])], [text("--no-create-info")])
        .arg_when(&[schema.clone(), on("routines")], [text("--routines")])
        .arg_when(
            &[schema.clone(), is("triggers", &["false"])],
            [text("--skip-triggers")],
        )
        .arg_when(&[schema.clone(), on("events")], [text("--events")])
        .arg_when(
            &[data.clone(), is("inserts", &["complete"])],
            [text("--complete-insert")],
        )
        .arg_when(
            &[data.clone(), is("inserts", &["single"])],
            [text("--skip-extended-insert")],
        )
        .arg_when(
            &[data.clone(), is("conflicts", &["ignore"])],
            [text("--insert-ignore")],
        )
        .arg_when(
            &[data.clone(), is("conflicts", &["replace"])],
            [text("--replace")],
        )
        .arg_when(&[data, on("hex-blob")], [text("--hex-blob")])
        .arg_when(
            &[schema, is("drop-tables", &["false"])],
            [text("--skip-add-drop-table")],
        )
        .arg_when(
            &[on("create-database"), on("drop-database")],
            [text("--add-drop-database")],
        )
        .flag("single-transaction", true, "--single-transaction")
        .flag("no-tablespaces", true, "--no-tablespaces")
        .arg_when(
            &[is("client", &["mysql"]), not("gtid", &["auto"])],
            [
                text("--set-gtid-purged="),
                mapped(
                    "gtid",
                    &[("off", "OFF"), ("on", "ON"), ("comment", "COMMENTED")],
                ),
            ],
        )
        .each(
            &[],
            "exclude-tables",
            [
                text("--ignore-table="),
                Piece::Database,
                text("."),
                value("exclude-tables"),
            ],
        );
    common(command)
        .extra("extra")
        // The names last, after `--`, so none reads as an option:
        // `--databases db` creates the database, `db [tables]` does not.
        .arg_when(&[on("create-database")], [text("--databases")])
        .arg([text("--")])
        .arg([Piece::Database])
        .each(
            &[is("create-database", &["false"])],
            "tables",
            [value("tables")],
        )
}

/// `mysql` reads the script from standard input.
fn restore_command() -> Command {
    let command = Command::new("mysql")
        .stdin_from_path()
        .arg([text("--batch")])
        .flag("force", true, "--force")
        .arg_when(
            &[is("client", &["mysql"]), on("sandbox")],
            [text("--system-command=OFF")],
        )
        .arg_when(
            &[is("client", &["mariadb"]), on("sandbox")],
            [text("--sandbox")],
        );
    common(command).extra("extra")
}

/// MySQL 8.4 LTS client programs, from Oracle's own archives: `mysqldump`,
/// `mysql` and the OpenSSL libraries they load. Oracle publishes nothing
/// within Violet's limits (`.tar.gz` or `.tar.xz`, 256 MiB) for Linux on
/// ARM (821 MiB) or Windows (a 268 MiB `.zip`): there Violet uses the
/// programs installed on the machine.
pub fn downloads() -> Vec<Download> {
    const VERSION: &str = "8.4.11";
    const BASE: &str = "https://cdn.mysql.com/Downloads/MySQL-8.4";
    let download =
        |platform: &str, name: &str, extension: &str, sha256: &str, size: u64, libs: &[&str]| {
            let root = format!("mysql-{VERSION}-{name}");
            let mut extract = vec!["bin/mysqldump".to_owned(), "bin/mysql".to_owned()];
            extract.extend(libs.iter().map(|lib| (*lib).to_owned()));
            Download {
                platform: platform.into(),
                url: format!("{BASE}/{root}.{extension}"),
                sha256: sha256.into(),
                size,
                label: format!("MySQL {VERSION} client"),
                root,
                extract,
                bin: "bin".into(),
            }
        };
    let macos = ["lib/libssl.3.dylib", "lib/libcrypto.3.dylib"];
    vec![
        download(
            "macos-aarch64",
            "macos15-arm64",
            "tar.gz",
            "b96e00493bc3499b9ffd7f08d65c5d64933af0383a8287d9873b64f94c2d6009",
            167_977_240,
            &macos,
        ),
        download(
            "macos-x86_64",
            "macos15-x86_64",
            "tar.gz",
            "90e8aea10698d01b978f0179e72e8d5e2cbe9f9bd5771e88b0b75d7c82244d3f",
            171_974_960,
            &macos,
        ),
        download(
            "linux-x86_64",
            "linux-glibc2.28-x86_64-minimal",
            "tar.xz",
            "383f54e124d5f325d67f0c6912a8f96814eedc761a17ea30112e52fa4cc6b143",
            79_777_440,
            &["lib/private/libssl.so.3", "lib/private/libcrypto.so.3"],
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use violet_sdk::tools::CommandPart;

    /// Every condition and value names a declared setting (Violet refuses
    /// the task otherwise; this catches it before release).
    #[test]
    fn templates_name_declared_settings() {
        for task in tasks() {
            let ids: Vec<&str> = task.fields.iter().map(|f| f.id.as_str()).collect();
            let known = |field: &str| field == "@ssl" || ids.contains(&field);
            for field in &task.fields {
                for condition in &field.when {
                    assert!(known(&condition.field), "{}", condition.field);
                }
            }
            for command in &task.commands {
                assert!(task.programs.contains(&command.program));
                for condition in &command.when {
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
                            if let Some(field) = &argument.each {
                                assert!(known(field), "{field}");
                            }
                        }
                        CommandPart::Extra(field) => assert!(known(field)),
                        CommandPart::Objects(_) => {}
                    }
                }
            }
            if let Some(field) = &task.connection.database_field {
                assert!(known(field));
            }
        }
    }

    #[test]
    fn downloads_keep_the_programs_and_their_libraries() {
        let downloads = downloads();
        assert_eq!(downloads.len(), 3);
        for download in &downloads {
            assert!(download.url.ends_with(".tar.gz") || download.url.ends_with(".tar.xz"));
            assert!(download.url.contains(&download.root));
            assert_eq!(download.sha256.len(), 64);
            assert!(download.extract.iter().any(|e| e == "bin/mysqldump"));
            assert!(download.extract.iter().any(|e| e.contains("libssl")));
        }
    }
}
