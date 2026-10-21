//! A quick backup of the whole server to an `.rdb` file, with
//! `redis-cli --rdb` or `valkey-cli --rdb` (either reads both servers).
//! Violet downloads Valkey's client on Linux; elsewhere it uses the one
//! installed.

use violet_sdk::tools::{
    Binding, Command, Connection, Download, Field, Target, Task, TaskKind, is, path, text,
};

/// Where the clients usually are, besides `PATH`.
const LOCATIONS: &[&str] = &[
    "/opt/homebrew/bin",
    "/usr/local/bin",
    "/usr/bin",
    r"C:\Program Files\Redis",
    r"C:\Program Files\Memurai",
];

const CLIENTS: &[(&str, &str)] = &[("valkey-cli", "valkey-cli"), ("redis-cli", "redis-cli")];

fn command(program: &str) -> Command {
    Command::new(program)
        .when(is("client", &[program]))
        .arg_when(&[is("@ssl", &["require"])], [text("--tls")])
        .arg([text("--rdb")])
        .arg([path()])
}

pub fn tasks() -> Vec<Task> {
    vec![
        Task::new(TaskKind::Backup, &["valkey-cli", "redis-cli"])
            .locations(LOCATIONS)
            .field(
                Field::choice("client", "Client program", "Connection", "valkey-cli", CLIENTS)
                    .help("Either backs up Redis and Valkey alike. Violet downloads valkey-cli on Linux."),
            )
            .connection(Connection {
                // `redis-cli` takes no `-hhost`: the value is an argument of
                // its own.
                host: Some(Binding::Prefix("-h ".into())),
                port: Some(Binding::Prefix("-p ".into())),
                user: Some(Binding::Prefix("--user ".into())),
                // `--rdb` copies every database at once.
                database: None,
                password_env: Some("REDISCLI_AUTH".into()),
                database_field: None,
            })
            .target(Target::file("rdb"))
            .command(command("valkey-cli"))
            .command(command("redis-cli")),
    ]
}

pub fn downloads() -> Vec<Download> {
    const VERSION: &str = "8.1.4";
    let linux = |platform: &str, arch: &str, sha256: &str, size: u64| {
        let root = format!("valkey-{VERSION}-jammy-{arch}");
        Download {
            platform: platform.into(),
            url: format!("https://download.valkey.io/releases/{root}.tar.gz"),
            sha256: sha256.into(),
            size,
            label: format!("Valkey {VERSION} client"),
            root,
            extract: vec!["bin/valkey-cli".into()],
            bin: "bin".into(),
        }
    };
    vec![
        linux(
            "linux-x86_64",
            "x86_64",
            "d536d948483e3742780da9bb0cdc8f192ccd84dffc42505d29ccc412d4a55fac",
            35_957_210,
        ),
        linux(
            "linux-aarch64",
            "arm64",
            "4a70e88c5781532309141ad02e3026c004c33f61d24c4ac489891318cf6e460a",
            35_908_260,
        ),
    ]
}
