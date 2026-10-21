//! The Redis provider, run through the host as Violet runs it, against a
//! real server. Skipped unless `VIOLET_TEST_REDIS` names one, as
//! `host:port:user:password` (user and password may be empty):
//!
//! ```sh
//! docker run -d -p 6390:6379 redis:8 --requirepass violet
//! cargo build --release --target wasm32-wasip2 --manifest-path providers/redis/Cargo.toml
//! VIOLET_TEST_REDIS=127.0.0.1:6390::violet cargo test -p violet-wasm --test redis
//! ```
//!
//! Each test works in its own database, emptied first.

use std::path::PathBuf;

use violet_core::{
    ConnectionProfile, Credentials, Driver, DriverId, ErrorKind, ExecLimits, Language, Login,
    ResultSet, Secret, Session,
};
use violet_wasm::{Runtime, WasmDriver};

fn provider_wasm() -> Option<Vec<u8>> {
    let path = std::env::var_os("VIOLET_TEST_REDIS_PROVIDER").map_or_else(
        || {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(
                "../../providers/redis/target/wasm32-wasip2/release/violet_provider_redis.wasm",
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

struct Server {
    driver: WasmDriver,
    profile: ConnectionProfile,
    password: Option<String>,
}

async fn server(db: u32) -> Option<Server> {
    let Ok(target) = std::env::var("VIOLET_TEST_REDIS") else {
        eprintln!("VIOLET_TEST_REDIS not set; skipping");
        return None;
    };
    let wasm = provider_wasm()?;
    let mut parts = target.splitn(4, ':');
    let host = parts.next().unwrap_or("127.0.0.1").to_owned();
    let port = parts.next().and_then(|p| p.parse().ok()).unwrap_or(6379);
    let user = parts.next().unwrap_or_default().to_owned();
    let password = parts.next().filter(|p| !p.is_empty()).map(str::to_owned);
    let runtime = Runtime::new(None).expect("runtime");
    let info = runtime.inspect(wasm.clone()).await.expect("inspect").info;
    let mut profile = ConnectionProfile::new(DriverId::new("redis"), port);
    profile.host = host;
    profile.username = user;
    profile.database = format!("db{db}");
    let server = Server {
        driver: WasmDriver::new(runtime, info, Box::new(move || Ok(wasm.clone()))),
        profile,
        password,
    };
    let session = server.connect().await;
    run(&*session, "FLUSHDB").await;
    session.close().await;
    Some(server)
}

impl Server {
    async fn connect(&self) -> Box<dyn Session> {
        let credentials = Credentials {
            password: self.password.clone().map(Secret::new),
        };
        self.driver
            .connect(&self.profile, &credentials)
            .await
            .expect("connect")
    }
}

async fn run(session: &dyn Session, script: &str) -> Vec<ResultSet> {
    session
        .execute(script, ExecLimits::default())
        .await
        .unwrap_or_else(|error| panic!("{script}: {error}"))
}

fn cells(set: &ResultSet) -> Vec<Vec<Option<String>>> {
    set.rows
        .iter()
        .map(|row| {
            (0..set.columns.len())
                .map(|i| row.get(i).flatten().map(str::to_owned))
                .collect()
        })
        .collect()
}

fn names(set: &ResultSet) -> Vec<&str> {
    set.columns.iter().map(|c| c.name.as_str()).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn describes_itself_as_redis() {
    let Some(wasm) = provider_wasm() else { return };
    let runtime = Runtime::new(None).expect("runtime");
    let info = runtime.inspect(wasm).await.expect("inspect").info;
    assert_eq!(info.id.as_str(), "redis");
    assert_eq!(info.language, Language::Redis);
    assert_eq!(
        (info.user, info.password),
        (Login::Optional, Login::Optional)
    );
    assert_eq!(info.default_port, 6379);
    assert_eq!(violet_wasm::api_level(&info), "0.2.0");
}

#[tokio::test(flavor = "multi_thread")]
async fn connects_and_runs_scripts() {
    let Some(server) = server(1).await else {
        return;
    };
    let session = server.connect().await;
    let info = session.server_info().await.expect("info");
    assert_eq!(info.database, "db1");
    assert!(!info.version.is_empty());
    assert_eq!(session.language(), Language::Redis);

    let sets = run(
        &*session,
        "SET greeting \"hello world\"\n# a comment\nGET greeting\nHSET h a 1 b 2\nHGETALL h\nINCR n",
    )
    .await;
    assert_eq!(sets.len(), 5);
    assert_eq!(sets[0].command.as_deref(), Some("OK"));
    assert_eq!(cells(&sets[1]), [[Some("hello world".to_owned())]]);
    assert_eq!(names(&sets[3]), ["field", "value"]);
    assert_eq!(sets[3].rows.len(), 2);
    assert!(sets[4].columns[0].numeric);

    let error = session
        .execute("GET greeting\nHGET greeting x", ExecLimits::default())
        .await
        .expect_err("wrong type");
    assert_eq!(error.code.as_deref(), Some("WRONGTYPE"));
    assert_eq!(error.position, Some(14));
    // The session survives errors.
    run(&*session, "PING").await;

    let refused = session
        .execute("BLPOP q 0", ExecLimits::default())
        .await
        .expect_err("blocking");
    assert_eq!(refused.kind, ErrorKind::Unsupported);
    session.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn transactions_queue_and_name_their_results() {
    let Some(server) = server(2).await else {
        return;
    };
    let session = server.connect().await;
    let sets = run(&*session, "MULTI\nSET a 1\nINCR a\nGET a\nEXEC").await;
    let shown: Vec<Option<&str>> = sets.iter().map(|s| s.command.as_deref()).collect();
    assert_eq!(shown, [Some("OK"), None, None]);
    assert_eq!(cells(&sets[2]), [[Some("2".to_owned())]]);
    // One line at a time, as running the statement under the cursor does.
    run(&*session, "MULTI").await;
    run(&*session, "INCR a").await;
    let sets = run(&*session, "EXEC").await;
    assert_eq!(cells(&sets[0]), [[Some("3".to_owned())]]);

    run(&*session, "SELECT 3").await;
    assert_eq!(session.server_info().await.expect("info").database, "db3");

    // The tree's changes run in their own database, not the editor's.
    let sets = session
        .run_in(
            "db2",
            "MULTI
SET b 1
EXPIRE b 100
EXEC
TTL b",
            ExecLimits::default(),
        )
        .await
        .expect("run in db2");
    assert_eq!(cells(sets.last().expect("ttl")), [[Some("100".to_owned())]]);
    assert_eq!(session.server_info().await.expect("info").database, "db3");
    let open = session
        .run_in(
            "db2",
            "MULTI
SET c 1",
            ExecLimits::default(),
        )
        .await
        .expect_err("left open");
    assert_eq!(open.kind, ErrorKind::Query);
    session.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn lists_keys_a_page_at_a_time() {
    let Some(server) = server(4).await else {
        return;
    };
    let session = server.connect().await;
    let mut script = String::new();
    for n in 0..1200 {
        script.push_str(&format!("SET user:{n} x\n"));
    }
    script.push_str(
        "HSET h f v\nRPUSH l a b c\nSADD s m\nZADD z 1 m\nXADD st * k v\nEXPIRE h 3600\n",
    );
    run(&*session, &script).await;

    let schemas = session.schemas().await.expect("schemas");
    assert!(schemas.contains(&"db4".to_owned()), "{schemas:?}");

    let mut seen = std::collections::BTreeSet::new();
    let mut cursor: Option<String> = None;
    loop {
        let page = session
            .entries("db4", Some("user:*"), cursor.as_deref(), 500)
            .await
            .expect("entries");
        assert!(page.objects.len() <= 1064);
        for object in &page.objects {
            assert_eq!(object.subtype.as_deref(), Some("string"));
            seen.insert(object.name.clone());
        }
        cursor = page.next;
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(seen.len(), 1200);

    let mut all = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let page = session
            .entries("db4", None, cursor.as_deref(), 500)
            .await
            .expect("entries");
        all.extend(page.objects);
        cursor = page.next;
        if cursor.is_none() {
            break;
        }
    }
    let find = |name: &str| all.iter().find(|o| o.name == name).cloned();
    let hash = find("h").expect("hash");
    assert_eq!(hash.subtype.as_deref(), Some("hash"));
    assert_eq!(hash.estimated_rows, Some(1));
    assert!(
        hash.detail.as_deref().is_some_and(|d| d.contains("TTL")),
        "{hash:?}"
    );
    for (name, subtype) in [("l", "list"), ("s", "set"), ("z", "zset"), ("st", "stream")] {
        assert_eq!(find(name).and_then(|o| o.subtype).as_deref(), Some(subtype));
    }
    let columns = session.columns("db4", "z").await.expect("columns");
    assert_eq!(
        columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
        ["member", "score"]
    );
    session.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn data_views_page_without_gaps() {
    let Some(server) = server(5).await else {
        return;
    };
    let session = server.connect().await;
    let mut script = String::from("HSET big");
    for n in 0..2500 {
        script.push_str(&format!(" f{n} {n}"));
    }
    script.push_str("\nRPUSH list");
    for n in 0..450 {
        script.push_str(&format!(" {n}"));
    }
    run(&*session, &script).await;
    // Browsing runs on its own connection: the editor's database and open
    // transaction do not matter.
    run(&*session, "SELECT 0\nMULTI").await;

    let mut fields = std::collections::BTreeSet::new();
    let mut offset = 0;
    loop {
        let sets = session
            .page("db5", "HGETALL big", offset, 200)
            .await
            .expect("page");
        let rows = cells(&sets[0]);
        let shown = rows.len().min(200);
        for row in &rows[..shown] {
            assert!(
                fields.insert(row[0].clone().expect("field")),
                "duplicate {row:?}"
            );
        }
        offset += shown as u64;
        if rows.len() <= 200 {
            break;
        }
    }
    assert_eq!(fields.len(), 2500);
    assert_eq!(
        session.count("db5", "HGETALL big").await.expect("count"),
        Some(2500)
    );

    let sets = session
        .page("db5", "LRANGE list 0 -1", 400, 100)
        .await
        .expect("list page");
    let rows = cells(&sets[0]);
    assert_eq!(rows.len(), 50);
    assert_eq!(rows[0], [Some("400".to_owned()), Some("400".to_owned())]);
    assert_eq!(
        session
            .count("db5", "LRANGE list 0 -1")
            .await
            .expect("count"),
        Some(450)
    );
    run(&*session, "DISCARD").await;
    session.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_wrong_password_is_an_authentication_error() {
    let Some(mut server) = server(6).await else {
        return;
    };
    if server.password.is_none() {
        return;
    }
    server.password = Some("wrong".into());
    let credentials = Credentials {
        password: server.password.clone().map(Secret::new),
    };
    let error = server
        .driver
        .connect(&server.profile, &credentials)
        .await
        .err()
        .expect("refused");
    assert_eq!(error.kind, ErrorKind::Authentication);
}
