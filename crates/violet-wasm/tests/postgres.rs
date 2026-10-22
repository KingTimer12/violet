//! The PostgreSQL provider, run through the host as Violet runs it.
//!
//! Needs the provider built (skipped otherwise):
//!
//! ```sh
//! cargo build --release --target wasm32-wasip2 \
//!     --manifest-path providers/postgres/Cargo.toml
//! ```
//!
//! The live tests also need a server, named by `VIOLET_TEST_PG` as
//! `host:port:user:password:database`:
//!
//! ```sh
//! docker run -d --rm -e POSTGRES_PASSWORD=violet -e POSTGRES_USER=violet \
//!     -p 55499:5432 postgres:17-alpine
//! VIOLET_TEST_PG=localhost:55499:violet:violet:violet cargo test -p violet-wasm
//! ```

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use violet_core::{
    ConnectionProfile, Credentials, Driver, DriverId, ErrorKind, ExecLimits, ObjectKind, ObjectRef,
    OrderBy, Paging, Row, Secret, Session, SslMode, TaskKind, TaskRequest,
};
use violet_wasm::{Runtime, WasmDriver};

fn provider_wasm() -> Option<Vec<u8>> {
    let path = std::env::var_os("VIOLET_TEST_PROVIDER").map_or_else(
        || {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(
                "../../providers/postgres/target/wasm32-wasip2/release/violet_provider_postgres.wasm",
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

fn runtime() -> Runtime {
    Runtime::new(None).expect("runtime")
}

async fn driver() -> Option<WasmDriver> {
    let wasm = provider_wasm()?;
    let runtime = runtime();
    let info = runtime.inspect(wasm.clone()).await.expect("inspect").info;
    Some(WasmDriver::new(
        runtime,
        info,
        Box::new(move || Ok(wasm.clone())),
    ))
}

fn target() -> Option<(ConnectionProfile, Credentials)> {
    let spec = std::env::var("VIOLET_TEST_PG").ok()?;
    let parts: Vec<&str> = spec.split(':').collect();
    let [host, port, user, password, database] = parts.as_slice() else {
        panic!("VIOLET_TEST_PG must be host:port:user:password:database");
    };
    let mut profile =
        ConnectionProfile::new(DriverId::new("postgres"), port.parse().expect("port"));
    profile.host = (*host).to_owned();
    profile.username = (*user).to_owned();
    profile.database = (*database).to_owned();
    profile.ssl = SslMode::Disable;
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
    assert_eq!(info.id.as_str(), "postgres");
    assert_eq!(info.name, "PostgreSQL");
    assert_eq!(info.default_port, 5432);
    assert!(info.capabilities.cancel && info.capabilities.databases);
    assert_eq!(info.dialect.paging, Paging::LimitOffset);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_version_comes_from_the_crate() {
    let Some(wasm) = provider_wasm() else {
        return;
    };
    let description = runtime().inspect(wasm).await.expect("inspect");
    assert_eq!(description.version, "0.1.0");
    assert!(description.description.contains("PostgreSQL"));
}

#[tokio::test(flavor = "multi_thread")]
async fn backups_and_restores_are_declared_and_filled_by_violet() {
    let Some(driver) = driver().await else {
        return;
    };
    let tasks = driver.tasks().await.expect("tasks");
    let kinds: Vec<TaskKind> = tasks.iter().map(|task| task.kind).collect();
    assert_eq!(kinds, [TaskKind::Backup, TaskKind::Restore]);
    let backup = &tasks[0];
    assert!(backup.objects);
    assert_eq!(backup.programs, ["pg_dump"]);

    let mut profile = ConnectionProfile::new(DriverId::new("postgres"), 5432);
    profile.host = "db.example.com".into();
    profile.username = "app".into();
    profile.database = "shop".into();
    profile.ssl = SslMode::Require;
    let mut settings = backup.defaults();
    settings.insert("format".into(), "directory".into());
    settings.insert("jobs".into(), "4".into());
    settings.insert("exclude-data".into(), "logs.*, audit".into());
    let request = TaskRequest {
        kind: TaskKind::Backup,
        settings,
        objects: vec![ObjectRef {
            schema: "public".into(),
            name: Some("orders".into()),
        }],
        path: "/tmp/shop".into(),
    };
    let command = backup.build(&profile, &request).expect("command");
    assert_eq!(command.program, "pg_dump");
    assert_eq!(command.password_env.as_deref(), Some("PGPASSWORD"));
    for arg in [
        "--format=d",
        "--jobs=4",
        "--compress=6",
        "--file=/tmp/shop",
        "--exclude-table-data=logs.*",
        "--exclude-table-data=audit",
        "--table=\"public\".\"orders\"",
        "--verbose",
    ] {
        assert!(
            command.args.iter().any(|a| a == arg),
            "{arg} in {:?}",
            command.args
        );
    }
    let env = |name: &str| {
        command
            .env
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    };
    assert_eq!(env("PGHOST"), Some("db.example.com"));
    assert_eq!(env("PGDATABASE"), Some("shop"));
    assert_eq!(env("PGSSLMODE"), Some("verify-full"));
    assert_eq!(env("PGSSLROOTCERT"), Some("system"));

    // Restores: psql for plain SQL, pg_restore with --dbname otherwise, into
    // another database when asked.
    let restore = &tasks[1];
    let mut settings = restore.defaults();
    settings.insert("database".into(), "copy".into());
    settings.insert("format".into(), "plain".into());
    let mut request = TaskRequest {
        kind: TaskKind::Restore,
        settings,
        objects: Vec::new(),
        path: "/tmp/shop.sql".into(),
    };
    let psql = restore.build(&profile, &request).expect("psql");
    assert_eq!(psql.program, "psql");
    assert!(psql.args.contains(&"--dbname=copy".to_owned()));
    assert!(psql.args.contains(&"--file=-".to_owned()));
    assert!(psql.stdin_from_path);
    assert_eq!(psql.stdin_lines.len(), 2);
    request.settings.insert("format".into(), "custom".into());
    let pg_restore = restore.build(&profile, &request).expect("pg_restore");
    assert_eq!(pg_restore.program, "pg_restore");
    let n = pg_restore.args.len();
    assert_eq!(pg_restore.args[n - 2..], ["--", "/tmp/shop.sql"]);
    // A database typed as a connection string is refused, not followed.
    request
        .settings
        .insert("database".into(), "host=evil dbname=x".into());
    assert!(restore.build(&profile, &request).is_err());

    let downloads = driver.downloads().await.expect("downloads");
    assert!(downloads.iter().any(|d| d.platform == "macos-aarch64"));

    let format = driver
        .inspect_input(b"PGDMP\x01", &[], false)
        .await
        .expect("inspect");
    assert_eq!(format.get("format").map(String::as_str), Some("custom"));
}

/// A provider built before backup and restore still loads, with none.
#[tokio::test(flavor = "multi_thread")]
async fn providers_without_tools_still_run() {
    let Some(path) = std::env::var_os("VIOLET_TEST_OLD_PROVIDER") else {
        return;
    };
    let wasm = std::fs::read(path).expect("old provider");
    let runtime = runtime();
    let info = runtime.inspect(wasm.clone()).await.expect("inspect").info;
    let driver = WasmDriver::new(runtime, info, Box::new(move || Ok(wasm.clone())));
    assert!(driver.tasks().await.expect("tasks").is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn garbage_is_not_a_provider() {
    let error = runtime()
        .inspect(b"\0asm not really".to_vec())
        .await
        .expect_err("refused");
    assert_eq!(error.kind, ErrorKind::Provider);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_closed_port_is_a_connection_error() {
    let Some(driver) = driver().await else {
        return;
    };
    let mut profile = ConnectionProfile::new(DriverId::new("postgres"), 1);
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
async fn browses_queries_and_cancels() {
    let (Some((profile, credentials)), Some(driver)) = (target(), driver().await) else {
        eprintln!("VIOLET_TEST_PG not set; skipping");
        return;
    };
    let session = driver
        .connect(&profile, &credentials)
        .await
        .expect("connect");

    let server = session.server_info().await.expect("server info");
    assert!(server.version.starts_with("PostgreSQL "), "{server:?}");
    assert_eq!(server.user, profile.username);

    session
        .execute(
            "DROP SCHEMA IF EXISTS violet_test CASCADE;
             CREATE SCHEMA violet_test;
             CREATE TABLE violet_test.people (id serial PRIMARY KEY, name text NOT NULL, born date);
             COMMENT ON TABLE violet_test.people IS 'humans';
             CREATE VIEW violet_test.adults AS SELECT * FROM violet_test.people;
             CREATE FUNCTION violet_test.twice(x int) RETURNS int AS $$ SELECT x * 2; $$ LANGUAGE sql;
             INSERT INTO violet_test.people (name, born) VALUES ('Ada', '1815-12-10'), ('Alan', NULL);",
            ExecLimits::default(),
        )
        .await
        .expect("setup");

    assert!(
        session
            .databases()
            .await
            .expect("databases")
            .contains(&profile.database)
    );
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
    assert!(kinds.contains(&("people_id_seq", ObjectKind::Sequence)));
    assert!(kinds.contains(&("twice(x integer)", ObjectKind::Function)));
    let people = objects.iter().find(|o| o.name == "people").expect("people");
    assert_eq!(people.comment.as_deref(), Some("humans"));
    assert_eq!(people.schema, "violet_test");

    // Names travel as parameters: a quote in one cannot break the query.
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
    assert_eq!(columns.len(), 3);
    assert!(columns[0].primary_key && !columns[0].nullable);
    assert!(
        columns[0]
            .default
            .as_deref()
            .is_some_and(|d| d.starts_with("nextval"))
    );
    assert_eq!(columns[2].data_type, "date");
    assert!(columns[2].nullable);

    // A single statement: typed columns, NULL kept apart from text.
    let sets = session
        .execute(
            "SELECT id, name, born FROM violet_test.people ORDER BY id",
            ExecLimits::default(),
        )
        .await
        .expect("select");
    assert_eq!(sets.len(), 1);
    let set = &sets[0];
    assert_eq!(set.columns[0].type_name.as_deref(), Some("int4"));
    assert_eq!(set.columns[2].type_name.as_deref(), Some("date"));
    assert!(set.columns[0].numeric && !set.columns[1].numeric);
    assert_eq!(set.command.as_deref(), Some("SELECT"));
    assert_eq!(set.affected, Some(2));
    assert_eq!(
        set.rows.row(0).map(Row::to_vec),
        Some(vec![
            Some("1".into()),
            Some("Ada".into()),
            Some("1815-12-10".into())
        ])
    );
    assert_eq!(set.rows.get(1, 2), Some(None));

    // Types the built-in table does not know are looked up by name.
    let sets = session
        .execute("SELECT 'a=>1'::text::tsvector", ExecLimits::default())
        .await
        .expect("tsvector");
    assert_eq!(sets[0].columns[0].type_name.as_deref(), Some("tsvector"));

    // Several statements: one set each, commands with counts.
    let sets = session
        .execute(
            "UPDATE violet_test.people SET name = name; SELECT 1 AS one; SELECT 1 WHERE false",
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

    // Row limit.
    let sets = session
        .execute("SELECT generate_series(1, 50)", ExecLimits { max_rows: 10 })
        .await
        .expect("limit");
    assert_eq!(sets[0].rows.len(), 10);
    assert!(sets[0].truncated);

    // Rows past the cap are read and dropped within a call's budget of
    // instructions, so later statements still run.
    let sets = session
        .execute(
            "SELECT generate_series(1, 2000000); SELECT 'after'",
            ExecLimits { max_rows: 10 },
        )
        .await
        .expect("big result");
    assert!(sets[0].truncated && sets[0].rows.len() == 10);
    assert_eq!(sets[1].rows.get(0, 0), Some(Some("after")));

    // Unicode survives the trip.
    let sets = session
        .execute("SELECT 'olá, 世界 🐘'", ExecLimits::default())
        .await
        .expect("unicode");
    assert_eq!(sets[0].rows.get(0, 0), Some(Some("olá, 世界 🐘")));

    // Errors carry SQLSTATE and position, and the session survives them.
    let error = session
        .execute("SELECT nope FROM violet_test.people", ExecLimits::default())
        .await
        .expect_err("bad column");
    assert_eq!(error.kind, ErrorKind::Query);
    assert_eq!(error.code.as_deref(), Some("42703"));
    assert_eq!(error.position, Some(8));
    session
        .execute("SELECT 1", ExecLimits::default())
        .await
        .expect("still usable");

    // COPY FROM STDIN is refused without hanging the session.
    let error = session
        .execute("COPY violet_test.people FROM STDIN", ExecLimits::default())
        .await
        .expect_err("copy in");
    assert_eq!(error.kind, ErrorKind::Unsupported, "{error:?}");

    // Cancellation, from a separate instance.
    let session: Arc<dyn Session> = Arc::from(session);
    let canceller = session.canceller().expect("cancel");
    let cancel = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        canceller.cancel().await
    });
    let error = session
        .execute("SELECT pg_sleep(10)", ExecLimits::default())
        .await
        .expect_err("cancelled");
    assert_eq!(error.kind, ErrorKind::Cancelled, "{error:?}");
    cancel.await.expect("join").expect("cancel sent");

    session
        .execute("DROP SCHEMA violet_test CASCADE", ExecLimits::default())
        .await
        .expect("cleanup");
    session.close().await;
    assert!(
        session.schemas().await.is_err(),
        "a closed session refuses work"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn closing_stops_a_running_statement() {
    let (Some((profile, credentials)), Some(driver)) = (target(), driver().await) else {
        return;
    };
    let session: Arc<dyn Session> = Arc::from(
        driver
            .connect(&profile, &credentials)
            .await
            .expect("connect"),
    );
    let running = {
        let session = Arc::clone(&session);
        tokio::spawn(async move {
            session
                .execute("SELECT pg_sleep(30)", ExecLimits::default())
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    let closed = tokio::time::timeout(Duration::from_secs(5), session.close()).await;
    assert!(closed.is_ok(), "close did not wait for the server");
    let result = tokio::time::timeout(Duration::from_secs(5), running)
        .await
        .expect("statement ended")
        .expect("join");
    assert_eq!(result.expect_err("stopped").kind, ErrorKind::Cancelled);
}

#[tokio::test(flavor = "multi_thread")]
async fn wrong_passwords_and_missing_tls_are_refused() {
    let (Some((mut profile, _)), Some(driver)) = (target(), driver().await) else {
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

    // The test server has no TLS: requiring it must fail before any
    // credentials are sent.
    profile.ssl = SslMode::Require;
    let error = driver
        .connect(&profile, &wrong)
        .await
        .err()
        .expect("no tls");
    assert_eq!(error.kind, ErrorKind::Tls, "{error:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn change_tokens_track_writes_and_paging_sql_runs() {
    let (Some((profile, credentials)), Some(driver)) = (target(), driver().await) else {
        return;
    };
    let reader = driver
        .connect(&profile, &credentials)
        .await
        .expect("reader");
    let writer = driver
        .connect(&profile, &credentials)
        .await
        .expect("writer");
    let run = |sql: &'static str| {
        let writer = &writer;
        async move { writer.execute(sql, ExecLimits::default()).await.expect(sql) }
    };
    run(
        "DROP SCHEMA IF EXISTS violet_cache CASCADE; CREATE SCHEMA violet_cache;
         CREATE TABLE violet_cache.t AS SELECT g AS n FROM generate_series(1, 450) g",
    )
    .await;

    // Statistics reach other sessions when the writer goes idle.
    async fn settled(session: &dyn Session) -> Option<String> {
        tokio::time::sleep(Duration::from_millis(1_200)).await;
        session.change_token().await.expect("token")
    }
    let before = settled(reader.as_ref()).await;
    assert!(before.is_some());
    assert_eq!(settled(reader.as_ref()).await, before, "stable while idle");
    run("UPDATE violet_cache.t SET n = n WHERE n = 1").await;
    let after_write = settled(reader.as_ref()).await;
    assert_ne!(after_write, before, "a write from another session");
    run("TRUNCATE violet_cache.t").await;
    assert_ne!(settled(reader.as_ref()).await, after_write, "a truncate");

    run("INSERT INTO violet_cache.t (n) SELECT g FROM generate_series(1, 450) g").await;
    let dialect = reader.dialect();
    let base = "SELECT n FROM violet_cache.t ORDER BY n";
    let paged = dialect.paged(base, 201, 400);
    let sets = reader
        .execute(&paged.sql, ExecLimits::default())
        .await
        .expect("page");
    assert_eq!(sets[0].rows.len(), 50);
    assert_eq!(sets[0].rows.get(0, 0), Some(Some("401")));
    let sets = reader
        .execute(&dialect.count(base), ExecLimits::default())
        .await
        .expect("count");
    assert_eq!(sets[0].rows.get(0, 0), Some(Some("450")));
    let browse = dialect.browse(
        "violet_cache",
        "t",
        Some("n > 440 -- trailing comment"),
        Some(&OrderBy {
            column: "n".into(),
            descending: true,
        }),
    );
    let sets = reader
        .execute(&browse, ExecLimits::default())
        .await
        .expect("browse");
    assert_eq!(sets[0].rows.len(), 10);
    assert_eq!(sets[0].rows.get(0, 0), Some(Some("450")));

    run("DROP SCHEMA violet_cache CASCADE").await;
    reader.close().await;
    writer.close().await;
}

/// A server with a self-signed certificate, named by `VIOLET_TEST_PG_TLS`
/// as `host:port`: the handshake runs and the certificate is refused.
#[tokio::test(flavor = "multi_thread")]
async fn untrusted_certificates_are_refused() {
    let (Ok(spec), Some(driver)) = (std::env::var("VIOLET_TEST_PG_TLS"), driver().await) else {
        return;
    };
    let (host, port) = spec.split_once(':').expect("host:port");
    let mut profile =
        ConnectionProfile::new(DriverId::new("postgres"), port.parse().expect("port"));
    profile.host = host.to_owned();
    profile.username = "violet".into();
    // Prefer on loopback: the server offers TLS, so it is used, and a
    // certificate that fails verification is an error, not a fallback.
    profile.ssl = SslMode::Prefer;
    let error = driver
        .connect(&profile, &Credentials::default())
        .await
        .err()
        .expect("refused");
    assert_eq!(error.kind, ErrorKind::Tls, "{error:?}");
    assert!(error.message.contains("certificate"), "{error}");
}
