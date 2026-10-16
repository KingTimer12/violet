//! The SQLite provider, run through the host as Violet runs it, on real
//! files.
//!
//! Needs the provider built (skipped otherwise), which takes the WASI SDK:
//!
//! ```sh
//! WASI_SDK_PATH=/opt/wasi-sdk scripts/package-providers.sh
//! ```

use std::path::{Path, PathBuf};

use violet_core::{
    ConnectionProfile, Credentials, Driver, DriverId, ErrorKind, ExecLimits, ObjectKind, Row,
    Session,
};
use violet_wasm::{Runtime, WasmDriver};

fn provider_wasm() -> Option<Vec<u8>> {
    let path = std::env::var_os("VIOLET_TEST_SQLITE_PROVIDER").map_or_else(
        || {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(
                "../../providers/sqlite/target/wasm32-wasip2/release/violet_provider_sqlite.wasm",
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

/// A fresh folder for one test's databases.
fn folder(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("violet-sqlite-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn profile(path: &Path) -> ConnectionProfile {
    let mut profile = ConnectionProfile::new(DriverId::new("sqlite"), 0);
    profile.host = path.to_string_lossy().into_owned();
    profile
}

async fn connect(driver: &WasmDriver, profile: &ConnectionProfile) -> Box<dyn Session> {
    driver
        .connect(profile, &Credentials::default())
        .await
        .expect("connect")
}

async fn run(session: &dyn Session, sql: &str) -> Vec<violet_core::ResultSet> {
    session
        .execute(sql, ExecLimits::default())
        .await
        .unwrap_or_else(|error| panic!("{sql}: {error}"))
}

#[tokio::test(flavor = "multi_thread")]
async fn the_provider_describes_itself_as_file_based() {
    let Some(driver) = driver().await else { return };
    let info = driver.info();
    assert_eq!(info.id.as_str(), "sqlite");
    assert_eq!(info.default_port, 0);
    assert!(info.capabilities.schemas && !info.capabilities.tls);
}

#[tokio::test(flavor = "multi_thread")]
async fn creates_browses_and_queries_a_database() {
    let Some(driver) = driver().await else { return };
    let path = folder("browse").join("shop.db");
    let profile = profile(&path);
    let session = connect(&driver, &profile).await;

    let server = session.server_info().await.expect("server info");
    assert!(server.version.starts_with("SQLite 3."), "{server:?}");
    assert_eq!(server.database, "shop.db");

    let sets = run(
        &*session,
        "CREATE TABLE people (
             id INTEGER PRIMARY KEY,
             name TEXT NOT NULL,
             height REAL DEFAULT 1.5,
             photo BLOB
         );
         CREATE VIEW tall AS SELECT * FROM people WHERE height > 1.8;
         INSERT INTO people (name, height, photo) VALUES ('Ada', 1.65, X'CAFE'), ('Alan', 1.9, NULL);",
    )
    .await;
    assert_eq!(sets.len(), 3);
    assert_eq!(sets[2].command.as_deref(), Some("INSERT"));
    assert_eq!(sets[2].affected, Some(2));
    assert!(path.is_file(), "the database is written to its file");

    assert_eq!(session.schemas().await.expect("schemas"), vec!["main"]);
    let objects = session.objects("main").await.expect("objects");
    let kinds: Vec<_> = objects.iter().map(|o| (o.name.as_str(), o.kind)).collect();
    assert_eq!(
        kinds,
        [("people", ObjectKind::Table), ("tall", ObjectKind::View)]
    );
    // A schema name cannot break out of its quotes.
    assert!(session.objects("main\" --").await.is_err());

    let columns = session.columns("main", "people").await.expect("columns");
    assert_eq!(columns.len(), 4);
    assert!(columns[0].primary_key && !columns[0].nullable);
    assert_eq!(columns[1].data_type, "TEXT");
    assert!(!columns[1].nullable);
    assert_eq!(columns[2].default.as_deref(), Some("1.5"));

    let sets = run(
        &*session,
        "SELECT id, name, height, photo, 1 + 1 AS two FROM people ORDER BY id",
    )
    .await;
    let set = &sets[0];
    assert_eq!(set.columns[0].type_name.as_deref(), Some("INTEGER"));
    assert!(set.columns[0].numeric && !set.columns[1].numeric && set.columns[2].numeric);
    assert!(
        set.columns[4].numeric,
        "expressions are typed by their values"
    );
    assert_eq!(
        set.rows.row(0).map(Row::to_vec),
        Some(vec![
            Some("1".into()),
            Some("Ada".into()),
            Some("1.65".into()),
            Some("0xCAFE".into()),
            Some("2".into()),
        ])
    );
    assert_eq!(set.rows.get(1, 3), Some(None));

    // Several statements, an empty one among them.
    let sets = run(
        &*session,
        "UPDATE people SET name = name || '!'; ; SELECT 'olá, 世界 🐬'",
    )
    .await;
    assert_eq!(sets.len(), 2);
    assert_eq!(sets[0].affected, Some(2));
    assert_eq!(sets[1].rows.get(0, 0), Some(Some("olá, 世界 🐬")));

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

    // Errors point at the statement, and the session survives them.
    let error = session
        .execute("SELECT 1; SELECT nope FROM people", ExecLimits::default())
        .await
        .expect_err("bad column");
    assert_eq!(error.kind, ErrorKind::Query);
    assert!(error.message.contains("nope"), "{error:?}");
    assert_eq!(error.position, Some(18));

    // Temporary tables live in memory, not beside the database.
    run(
        &*session,
        "CREATE TEMP TABLE scratch AS SELECT * FROM people; VACUUM",
    )
    .await;
    assert!(
        session
            .schemas()
            .await
            .expect("schemas")
            .contains(&"temp".to_owned())
    );

    let before = session.change_token().await.expect("token");
    run(&*session, "DELETE FROM people WHERE id = 1").await;
    assert_ne!(session.change_token().await.expect("token"), before);
    session.close().await;

    // Only the database is left: the journal went with the transaction.
    let files: Vec<_> = std::fs::read_dir(path.parent().expect("folder"))
        .expect("list")
        .map(|entry| entry.expect("entry").file_name())
        .collect();
    assert_eq!(files, ["shop.db"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn connections_share_the_database_and_take_turns() {
    let Some(driver) = driver().await else { return };
    let path = folder("turns").join("shared.db");
    let profile = profile(&path);
    let writer = connect(&driver, &profile).await;
    let reader = connect(&driver, &profile).await;
    run(&*writer, "CREATE TABLE t (x); INSERT INTO t VALUES (1)").await;
    let sets = run(&*reader, "SELECT count(*) FROM t").await;
    assert_eq!(sets[0].rows.get(0, 0), Some(Some("1")));

    // While one holds a write transaction, the other still reads the last
    // committed state, and cannot write.
    run(&*writer, "BEGIN IMMEDIATE; INSERT INTO t VALUES (2)").await;
    let sets = run(&*reader, "SELECT count(*) FROM t").await;
    assert_eq!(sets[0].rows.get(0, 0), Some(Some("1")));
    let started = std::time::Instant::now();
    let error = reader
        .execute(
            "PRAGMA busy_timeout = 100; INSERT INTO t VALUES (3)",
            ExecLimits::default(),
        )
        .await
        .expect_err("busy");
    assert!(error.message.contains("locked"), "{error:?}");
    assert!(error.hint.is_some());
    assert!(started.elapsed().as_secs() < 5);
    run(&*writer, "COMMIT").await;
    run(&*reader, "INSERT INTO t VALUES (3)").await;
    let sets = run(&*writer, "SELECT count(*) FROM t").await;
    assert_eq!(sets[0].rows.get(0, 0), Some(Some("3")));
    writer.close().await;
    reader.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn read_only_profiles_refuse_writes() {
    let Some(driver) = driver().await else { return };
    let path = folder("read-only").join("ro.db");
    let mut profile = profile(&path);
    let session = connect(&driver, &profile).await;
    run(&*session, "CREATE TABLE t (x)").await;
    session.close().await;

    profile.options.insert("read_only".into(), "true".into());
    let session = connect(&driver, &profile).await;
    run(&*session, "SELECT * FROM t").await;
    let error = session
        .execute("INSERT INTO t VALUES (1)", ExecLimits::default())
        .await
        .expect_err("read-only");
    assert!(error.message.contains("readonly"), "{error:?}");
    session.close().await;

    // A read-only profile does not create a missing file.
    let missing = profile_with(&path.with_file_name("missing.db"), true);
    assert!(
        driver
            .connect(&missing, &Credentials::default())
            .await
            .is_err()
    );
    assert!(!path.with_file_name("missing.db").exists());
}

fn profile_with(path: &Path, read_only: bool) -> ConnectionProfile {
    let mut profile = profile(path);
    if read_only {
        profile.options.insert("read_only".into(), "true".into());
    }
    profile
}

#[tokio::test(flavor = "multi_thread")]
async fn wal_databases_open_exclusively() {
    let Some(driver) = driver().await else { return };
    let path = folder("wal").join("wal.db");
    let profile = profile(&path);
    let session = connect(&driver, &profile).await;
    let sets = run(
        &*session,
        "PRAGMA locking_mode = EXCLUSIVE; PRAGMA journal_mode = WAL; CREATE TABLE t (x); \
         INSERT INTO t VALUES (1)",
    )
    .await;
    assert_eq!(sets[1].rows.get(0, 0), Some(Some("wal")));
    session.close().await;

    let session = connect(&driver, &profile).await;
    let sets = run(&*session, "SELECT count(*) FROM t").await;
    assert_eq!(sets[0].rows.get(0, 0), Some(Some("1")));
    session.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn other_files_stay_out_of_reach() {
    let Some(driver) = driver().await else { return };
    let dir = folder("reach");
    let other = dir.join("other.db");
    let session = connect(&driver, &profile(&dir.join("main.db"))).await;
    // ATTACH names another file: refused, and nothing is created.
    let error = session
        .execute(
            &format!("ATTACH DATABASE '{}' AS other", other.display()),
            ExecLimits::default(),
        )
        .await
        .expect_err("attach");
    assert!(error.message.contains("unable to open"), "{error:?}");
    assert!(!other.exists());
    // In-memory databases are fine.
    run(
        &*session,
        "ATTACH DATABASE ':memory:' AS scratch; CREATE TABLE scratch.t (x)",
    )
    .await;
    session.close().await;

    // Not a database: a clear error, and the file is left alone.
    let text = dir.join("notes.txt");
    std::fs::write(&text, "hello, this is not a database at all, not even close....................................................").expect("write");
    let error = driver
        .connect(&profile(&text), &Credentials::default())
        .await
        .err()
        .expect("not a database");
    assert_eq!(error.kind, ErrorKind::Connection);
    assert!(error.message.contains("not a SQLite database"), "{error:?}");
    assert!(
        std::fs::read_to_string(&text)
            .expect("read")
            .starts_with("hello")
    );
}
