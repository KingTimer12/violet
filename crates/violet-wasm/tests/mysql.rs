//! The MySQL provider, run through the host as Violet runs it.
//!
//! Needs the provider built (skipped otherwise):
//!
//! ```sh
//! cargo build --release --target wasm32-wasip2 \
//!     --manifest-path providers/mysql/Cargo.toml
//! ```
//!
//! The live tests also need a server, named by `VIOLET_TEST_MYSQL` as
//! `host:port:user:password:database`:
//!
//! ```sh
//! docker run -d --rm -e MYSQL_ROOT_PASSWORD=violet -e MYSQL_DATABASE=violet \
//!     -p 53306:3306 mysql:8.4
//! VIOLET_TEST_MYSQL=127.0.0.1:53306:root:violet:violet cargo test -p violet-wasm
//! ```

use std::path::PathBuf;

use violet_core::{
    ConnectionProfile, Credentials, Driver, DriverId, ErrorKind, ExecLimits, ObjectKind, Paging,
    Row, Secret, SslMode, TaskKind, TaskRequest,
};
use violet_wasm::{Runtime, WasmDriver};

fn provider_wasm() -> Option<Vec<u8>> {
    let path = std::env::var_os("VIOLET_TEST_MYSQL_PROVIDER").map_or_else(
        || {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(
                "../../providers/mysql/target/wasm32-wasip2/release/violet_provider_mysql.wasm",
            )
        },
        PathBuf::from,
    );
    let wasm = std::fs::read(&path).ok();
    if wasm.is_none() {
        eprintln!("{} not built; skipping", path.display());
    }
    wasm
}

async fn driver() -> Option<WasmDriver> {
    let wasm = provider_wasm()?;
    let runtime = Runtime::new(None).expect("runtime");
    let info = runtime.inspect(wasm.clone()).await.expect("inspect").info;
    Some(WasmDriver::new(
        runtime,
        info,
        Box::new(move || Ok(wasm.clone())),
    ))
}

/// The test server, over TLS when it offers it: a loopback host's
/// `Prefer` falls back to plain text when the certificate is not trusted,
/// as a fresh server's own is not.
fn target() -> Option<(ConnectionProfile, Credentials)> {
    let spec = std::env::var("VIOLET_TEST_MYSQL").ok()?;
    let parts: Vec<&str> = spec.split(':').collect();
    let [host, port, user, password, database] = parts.as_slice() else {
        panic!("VIOLET_TEST_MYSQL must be host:port:user:password:database");
    };
    let mut profile = ConnectionProfile::new(DriverId::new("mysql"), port.parse().expect("port"));
    profile.host = (*host).to_owned();
    profile.username = (*user).to_owned();
    profile.database = (*database).to_owned();
    profile.ssl = SslMode::Prefer;
    Some((
        profile,
        Credentials {
            password: Some(Secret::new(*password)),
        },
    ))
}

#[tokio::test(flavor = "multi_thread")]
async fn the_provider_describes_itself() {
    let Some(driver) = driver().await else {
        return;
    };
    let info = driver.info();
    assert_eq!(info.id.as_str(), "mysql");
    assert_eq!(info.name, "MySQL");
    assert_eq!(info.default_port, 3306);
    assert!(info.capabilities.schemas && info.capabilities.databases);
    assert_eq!(
        (info.dialect.quote_open, info.dialect.quote_close),
        ('`', '`')
    );
    assert_eq!(info.dialect.paging, Paging::LimitOffset);
}

#[tokio::test(flavor = "multi_thread")]
async fn backups_and_restores_are_declared_and_filled_by_violet() {
    let Some(driver) = driver().await else {
        return;
    };
    let tasks = driver.tasks().await.expect("tasks");
    let kinds: Vec<TaskKind> = tasks.iter().map(|task| task.kind).collect();
    assert_eq!(kinds, [TaskKind::Backup, TaskKind::Restore]);

    let mut profile = ConnectionProfile::new(DriverId::new("mysql"), 3306);
    profile.host = "db.example.com".into();
    profile.username = "app".into();
    profile.database = "shop".into();
    profile.ssl = SslMode::Require;

    let backup = &tasks[0];
    assert_eq!(backup.programs, ["mysqldump"]);
    let mut settings = backup.defaults();
    settings.insert("tables".into(), "orders, items".into());
    settings.insert("exclude-tables".into(), "logs".into());
    settings.insert("ssl-ca".into(), "/etc/ssl/cert.pem".into());
    let request = TaskRequest {
        kind: TaskKind::Backup,
        settings,
        objects: Vec::new(),
        path: "/tmp/shop.sql".into(),
    };
    let command = backup.build(&profile, &request).expect("command");
    assert_eq!(command.program, "mysqldump");
    assert_eq!(command.password_env.as_deref(), Some("MYSQL_PWD"));
    for arg in [
        "--host=db.example.com",
        "--port=3306",
        "--user=app",
        "--protocol=TCP",
        "--ssl-mode=VERIFY_IDENTITY",
        "--ssl-ca=/etc/ssl/cert.pem",
        "--result-file=/tmp/shop.sql",
        "--single-transaction",
        "--routines",
        "--hex-blob",
        "--no-tablespaces",
        "--ignore-table=shop.logs",
    ] {
        assert!(
            command.args.iter().any(|a| a == arg),
            "{arg} in {:?}",
            command.args
        );
    }
    // The names last, after `--`.
    let n = command.args.len();
    assert_eq!(command.args[n - 4..], ["--", "shop", "orders", "items"]);

    // The whole database, created by name.
    let mut request = request;
    request
        .settings
        .insert("create-database".into(), "true".into());
    request.settings.insert("ssl-ca".into(), String::new());
    let command = backup.build(&profile, &request).expect("command");
    let n = command.args.len();
    assert_eq!(command.args[n - 3..], ["--databases", "--", "shop"]);
    assert!(command.args.contains(&"--ssl-mode=REQUIRED".to_owned()));

    // Restores read the script from standard input, into another
    // database when asked.
    let restore = &tasks[1];
    assert_eq!(restore.programs, ["mysql"]);
    let mut settings = restore.defaults();
    settings.insert("database".into(), "copy".into());
    let mut request = TaskRequest {
        kind: TaskKind::Restore,
        settings,
        objects: Vec::new(),
        path: "/tmp/shop.sql".into(),
    };
    let mysql = restore.build(&profile, &request).expect("mysql");
    assert_eq!(mysql.program, "mysql");
    assert!(mysql.stdin_from_path);
    assert!(mysql.args.contains(&"--database=copy".to_owned()));
    assert!(mysql.args.contains(&"--system-command=OFF".to_owned()));
    assert!(mysql.args.contains(&"--binary-mode".to_owned()));
    // MariaDB's clients spell TLS and the sandbox their own way.
    request.settings.insert("client".into(), "mariadb".into());
    let mariadb = restore.build(&profile, &request).expect("mariadb");
    assert!(mariadb.args.contains(&"--ssl".to_owned()));
    assert!(mariadb.args.contains(&"--sandbox".to_owned()));
    assert!(!mariadb.args.iter().any(|a| a.starts_with("--ssl-mode")));
    // A database typed as an option is refused, not followed.
    request
        .settings
        .insert("database".into(), "x --host=evil".into());
    assert!(restore.build(&profile, &request).is_err());

    let downloads = driver.downloads().await.expect("downloads");
    let platforms: Vec<&str> = downloads.iter().map(|d| d.platform.as_str()).collect();
    assert_eq!(platforms, ["macos-aarch64", "macos-x86_64", "linux-x86_64"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_closed_port_is_a_connection_error() {
    let Some(driver) = driver().await else {
        return;
    };
    let mut profile = ConnectionProfile::new(DriverId::new("mysql"), 1);
    profile.username = "x".into();
    profile.ssl = SslMode::Disable;
    let error = driver
        .connect(&profile, &Credentials::default())
        .await
        .err()
        .expect("refused");
    assert_eq!(error.kind, ErrorKind::Connection, "{error:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn browses_and_queries() {
    let (Some((profile, credentials)), Some(driver)) = (target(), driver().await) else {
        eprintln!("VIOLET_TEST_MYSQL not set; skipping");
        return;
    };
    let session = driver
        .connect(&profile, &credentials)
        .await
        .expect("connect");

    let server = session.server_info().await.expect("server info");
    assert!(
        server.version.starts_with("MySQL ") || server.version.starts_with("MariaDB "),
        "{server:?}"
    );
    assert_eq!(server.user, profile.username);
    assert_eq!(server.database, profile.database);

    let sets = session
        .execute(
            "DROP DATABASE IF EXISTS violet_test;
             CREATE DATABASE violet_test;
             CREATE TABLE violet_test.people (
                 id int AUTO_INCREMENT PRIMARY KEY,
                 name varchar(40) NOT NULL COMMENT 'full name',
                 born date,
                 photo blob
             ) COMMENT 'humans';
             CREATE VIEW violet_test.adults AS SELECT * FROM violet_test.people;
             CREATE FUNCTION violet_test.twice(x int) RETURNS int DETERMINISTIC RETURN x * 2;
             INSERT INTO violet_test.people (name, born, photo)
                 VALUES ('Ada', '1815-12-10', X'CAFE'), ('Alan', NULL, NULL);",
            ExecLimits::default(),
        )
        .await
        .expect("setup");
    assert_eq!(sets.len(), 6);
    assert_eq!(sets[5].command.as_deref(), Some("INSERT"));
    assert_eq!(sets[5].affected, Some(2));

    assert!(
        session
            .schemas()
            .await
            .expect("schemas")
            .contains(&"violet_test".to_owned())
    );
    let objects = session.objects("violet_test").await.expect("objects");
    let kinds: Vec<_> = objects.iter().map(|o| (o.name.as_str(), o.kind)).collect();
    assert!(kinds.contains(&("people", ObjectKind::Table)));
    assert!(kinds.contains(&("adults", ObjectKind::View)));
    assert!(kinds.contains(&("twice", ObjectKind::Function)));
    let people = objects.iter().find(|o| o.name == "people").expect("people");
    assert_eq!(people.comment.as_deref(), Some("humans"));
    let adults = objects.iter().find(|o| o.name == "adults").expect("adults");
    assert_eq!(adults.comment, None);

    // Names reach the SQL as hex: a quote in one cannot break the query.
    assert!(
        session
            .objects("x' OR '1'='1")
            .await
            .expect("objects")
            .is_empty()
    );

    let columns = session
        .columns("violet_test", "people")
        .await
        .expect("columns");
    assert_eq!(columns.len(), 4);
    assert!(columns[0].primary_key && !columns[0].nullable);
    assert_eq!(columns[1].data_type, "varchar(40)");
    assert_eq!(columns[1].comment.as_deref(), Some("full name"));
    assert!(columns[2].nullable);

    // Typed columns, NULL kept apart from text, binary shown in hex.
    let sets = session
        .execute(
            "SELECT id, name, born, photo FROM violet_test.people ORDER BY id",
            ExecLimits::default(),
        )
        .await
        .expect("select");
    let set = &sets[0];
    assert_eq!(set.columns[0].type_name.as_deref(), Some("int"));
    assert_eq!(set.columns[2].type_name.as_deref(), Some("date"));
    assert_eq!(set.columns[3].type_name.as_deref(), Some("blob"));
    assert!(set.columns[0].numeric && !set.columns[1].numeric);
    assert_eq!(set.command.as_deref(), Some("SELECT"));
    assert_eq!(
        set.rows.row(0).map(Row::to_vec),
        Some(vec![
            Some("1".into()),
            Some("Ada".into()),
            Some("1815-12-10".into()),
            Some("0xCAFE".into()),
        ])
    );
    assert_eq!(set.rows.get(1, 2), Some(None));

    // Several statements: one set each.
    let sets = session
        .execute(
            "UPDATE violet_test.people SET name = CONCAT(name, '!'); SELECT 1 AS one; \
             SELECT 1 FROM DUAL WHERE false",
            ExecLimits::default(),
        )
        .await
        .expect("script");
    assert_eq!(sets.len(), 3);
    assert!(!sets[0].has_rows() && sets[0].affected == Some(2));
    assert_eq!(sets[0].command.as_deref(), Some("UPDATE"));
    assert_eq!(
        sets[1].rows.iter().map(Row::to_vec).collect::<Vec<_>>(),
        vec![vec![Some("1".into())]]
    );
    assert!(sets[2].has_rows() && sets[2].rows.is_empty());

    // Row limit, with later statements still run.
    let sets = session
        .execute(
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 1000) \
             SELECT i FROM n; SELECT 'after'",
            ExecLimits { max_rows: 10 },
        )
        .await
        .expect("limit");
    assert!(sets[0].truncated && sets[0].rows.len() == 10);
    assert_eq!(sets[1].rows.get(0, 0), Some(Some("after")));

    // Unicode survives the trip.
    let sets = session
        .execute("SELECT 'olá, 世界 🐬'", ExecLimits::default())
        .await
        .expect("unicode");
    assert_eq!(sets[0].rows.get(0, 0), Some(Some("olá, 世界 🐬")));

    // A statement past 16 MiB spans packets.
    let big = "x".repeat(17 << 20);
    let sets = session
        .execute(&format!("SELECT LENGTH('{big}')"), ExecLimits::default())
        .await
        .expect("big statement");
    assert_eq!(sets[0].rows.get(0, 0), Some(Some("17825792")));

    // Errors carry the server's code, and the session survives them.
    let error = session
        .execute("SELECT nope FROM violet_test.people", ExecLimits::default())
        .await
        .expect_err("bad column");
    assert_eq!(error.kind, ErrorKind::Query);
    assert_eq!(error.code.as_deref(), Some("1054"));
    session
        .execute("SELECT 1", ExecLimits::default())
        .await
        .expect("still usable");

    session
        .execute("DROP DATABASE violet_test", ExecLimits::default())
        .await
        .expect("cleanup");
    session.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn wrong_passwords_are_refused() {
    let (Some((profile, _)), Some(driver)) = (target(), driver().await) else {
        return;
    };
    let wrong = Credentials {
        password: Some(Secret::new("wrong")),
    };
    let error = driver
        .connect(&profile, &wrong)
        .await
        .err()
        .expect("rejected");
    assert_eq!(error.kind, ErrorKind::Authentication, "{error:?}");
}

/// With TLS off and the server's cache of verified passwords emptied,
/// `caching_sha2_password` needs the full exchange: the password goes
/// encrypted with the server's RSA key.
#[tokio::test(flavor = "multi_thread")]
async fn plain_connections_encrypt_the_password() {
    let (Some((mut profile, credentials)), Some(driver)) = (target(), driver().await) else {
        return;
    };
    let session = driver
        .connect(&profile, &credentials)
        .await
        .expect("connect");
    // Needs RELOAD; a server that refuses is still fine to test against.
    let flushed = session
        .execute("FLUSH PRIVILEGES", ExecLimits::default())
        .await
        .is_ok();
    session.close().await;
    profile.ssl = SslMode::Disable;
    let session = driver
        .connect(&profile, &credentials)
        .await
        .unwrap_or_else(|error| panic!("plain connection (cache flushed: {flushed}): {error:?}"));
    let sets = session
        .execute("SELECT 1", ExecLimits::default())
        .await
        .expect("query");
    assert_eq!(sets[0].rows.get(0, 0), Some(Some("1")));
    session.close().await;
}
