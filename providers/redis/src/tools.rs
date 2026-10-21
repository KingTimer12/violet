//! A quick backup of the whole server to an `.rdb` file, with
//! `redis-cli --rdb`.

use violet_sdk::tools::{
    Binding, Command, Connection, Download, Target, Task, TaskKind, is, path, text,
};

/// Where `redis-cli` usually is, besides `PATH`.
const LOCATIONS: &[&str] = &[
    "/opt/homebrew/bin",
    "/usr/local/bin",
    "/usr/bin",
    r"C:\Program Files\Redis",
    r"C:\Program Files\Memurai",
];

pub fn tasks() -> Vec<Task> {
    vec![
        Task::new(TaskKind::Backup, &["redis-cli"])
            .locations(LOCATIONS)
            .connection(Connection {
                host: Some(Binding::Prefix("-h ".into())),
                port: Some(Binding::Prefix("-p ".into())),
                user: Some(Binding::Prefix("--user ".into())),
                // `--rdb` copies every database at once.
                database: None,
                password_env: Some("REDISCLI_AUTH".into()),
                database_field: None,
            })
            .target(Target::file("rdb"))
            .command(
                Command::new("redis-cli")
                    .arg_when(&[is("@ssl", &["require"])], [text("--tls")])
                    .arg([text("--rdb")])
                    .arg([path()]),
            ),
    ]
}

pub fn downloads() -> Vec<Download> {
    Vec::new()
}
