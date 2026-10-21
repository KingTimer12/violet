//! The backend against a scripted driver: the result cache, paging, and
//! providers installed on demand.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use violet_app::{Backend, CachePolicy, DriverRegistry, Event, ExecOptions};
use violet_core::driver::AnsiDialect;
use violet_core::ports::{ProviderListing, ProviderStore};
use violet_core::{
    Canceller, Capabilities, ColumnInfo, ColumnMeta, ConnectionProfile, Credentials, DbError,
    DbObject, Dialect, Driver, DriverId, DriverInfo, ErrorKind, ExecLimits, Page, QueryOutput,
    Result, ResultSet, ServerInfo, Session, SqlDialect,
};
use violet_storage::{MemoryResultCache, MemorySecretStore};

/// What the fake server has seen and how it answers.
#[derive(Default)]
struct Server {
    executed: Mutex<Vec<String>>,
    /// The change token; bump it to simulate someone else writing.
    token: AtomicU64,
}

struct FakeDriver(Arc<Server>);
struct FakeSession(Arc<Server>);

/// A catalogue with one provider, the fake driver, installed on demand.
struct OnDemand {
    server: Arc<Server>,
    installs: AtomicU64,
}

#[async_trait]
impl ProviderStore for OnDemand {
    fn installed(&self) -> Result<Vec<Arc<dyn Driver>>> {
        Ok(Vec::new())
    }

    async fn available(&self) -> Result<Vec<ProviderListing>> {
        Ok(vec![ProviderListing {
            info: FakeDriver(Arc::clone(&self.server)).info(),
            version: "1.0.0".into(),
            description: String::new(),
            size: 1,
            installed: None,
        }])
    }

    async fn install(&self, id: &DriverId) -> Result<Arc<dyn Driver>> {
        if id.as_str() != "fake" {
            return Err(DbError::new(ErrorKind::Provider, "not in the catalogue"));
        }
        self.installs.fetch_add(1, Ordering::SeqCst);
        Ok(Arc::new(FakeDriver(Arc::clone(&self.server))))
    }

    async fn remove(&self, _: &DriverId) -> Result<()> {
        Ok(())
    }
}

#[async_trait]
impl Driver for FakeDriver {
    fn info(&self) -> DriverInfo {
        DriverInfo {
            id: DriverId::new("fake"),
            name: "Fake".into(),
            default_port: 1,
            default_database: String::new(),
            default_user: String::new(),
            capabilities: Capabilities {
                databases: false,
                schemas: true,
                cancel: false,
                tls: false,
            },
            dialect: SqlDialect::ANSI,
            language: violet_core::Language::Sql,
            user: violet_core::Login::Required,
            password: violet_core::Login::Optional,
            schema_label: String::new(),
        }
    }

    async fn connect(&self, _: &ConnectionProfile, _: &Credentials) -> Result<Box<dyn Session>> {
        Ok(Box::new(FakeSession(Arc::clone(&self.0))))
    }
}

#[async_trait]
impl Session for FakeSession {
    async fn server_info(&self) -> Result<ServerInfo> {
        Ok(ServerInfo {
            version: "Fake 1".into(),
            database: "db".into(),
            user: "me".into(),
        })
    }
    async fn databases(&self) -> Result<Vec<String>> {
        Ok(Vec::new())
    }
    async fn schemas(&self) -> Result<Vec<String>> {
        Ok(Vec::new())
    }
    async fn objects(&self, _: &str) -> Result<Vec<DbObject>> {
        Ok(Vec::new())
    }
    async fn columns(&self, _: &str, _: &str) -> Result<Vec<ColumnInfo>> {
        Ok(Vec::new())
    }

    /// Answers with a numbered row per requested `LIMIT`, up to 450 rows in
    /// all; `fail` errors at character 20 of what was sent.
    async fn execute(&self, sql: &str, _: ExecLimits) -> Result<Vec<ResultSet>> {
        self.0.executed.lock().expect("lock").push(sql.to_owned());
        if sql.contains("fail") {
            return Err(DbError {
                position: Some(20),
                ..DbError::new(ErrorKind::Query, "syntax error")
            });
        }
        if sql.contains("count(*)") {
            let mut set = ResultSet {
                columns: vec![column("count")],
                ..ResultSet::default()
            };
            set.rows.push([Some("450")]);
            return Ok(vec![set]);
        }
        let number = |key: &str| {
            sql.split(key)
                .nth(1)
                .and_then(|rest| rest.split_whitespace().next())
                .and_then(|n| n.parse::<u64>().ok())
        };
        let offset = number("OFFSET ").unwrap_or(0);
        let limit = number("LIMIT ").unwrap_or(450);
        let mut set = ResultSet {
            columns: vec![column("n")],
            ..ResultSet::default()
        };
        for n in offset..(offset + limit).min(450) {
            set.rows.push([Some(n.to_string().as_str())]);
        }
        Ok(vec![set])
    }

    fn canceller(&self) -> Option<Box<dyn Canceller>> {
        None
    }

    async fn change_token(&self) -> Result<Option<String>> {
        Ok(Some(self.0.token.load(Ordering::SeqCst).to_string()))
    }

    fn dialect(&self) -> &dyn Dialect {
        &AnsiDialect
    }

    async fn close(&self) {}
}

fn column(name: &str) -> ColumnMeta {
    ColumnMeta {
        name: name.into(),
        type_name: None,
        numeric: true,
    }
}

struct Harness {
    backend: Backend,
    server: Arc<Server>,
    profile: ConnectionProfile,
}

impl Harness {
    fn connected() -> Self {
        let server = Arc::new(Server::default());
        let registry = DriverRegistry::new().with(FakeDriver(Arc::clone(&server)));
        let backend = Backend::with_registry(
            registry,
            Arc::new(OnDemand {
                server: Arc::clone(&server),
                installs: AtomicU64::new(0),
            }),
            Arc::new(MemorySecretStore::default()),
            Arc::new(MemoryResultCache::default()),
            || {},
        )
        .expect("runtime");
        let mut profile = ConnectionProfile::new(DriverId::new("fake"), 1);
        profile.save_password = false;
        backend.connect(profile.clone(), None);
        let harness = Self {
            backend,
            server,
            profile,
        };
        harness.wait(|event| matches!(event, Event::Connected { .. }));
        harness
    }

    fn wait(&self, mut wanted: impl FnMut(&Event) -> bool) -> Event {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            for event in self.backend.drain() {
                if wanted(&event) {
                    return event;
                }
            }
            assert!(Instant::now() < deadline, "no such event");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn run(&self, sql: &str, options: ExecOptions) -> Result<QueryOutput> {
        let request = self
            .backend
            .execute(self.profile.id, sql.to_owned(), options);
        match self
            .wait(|event| matches!(event, Event::QueryFinished { request: r, .. } if *r == request))
        {
            Event::QueryFinished { result, .. } => result,
            _ => unreachable!(),
        }
    }

    fn executions(&self) -> usize {
        self.server.executed.lock().expect("lock").len()
    }
}

const READ: ExecOptions = ExecOptions {
    cache: CachePolicy::Use,
    page: None,
};

#[test]
fn unchanged_reads_come_from_the_cache() {
    let h = Harness::connected();
    let first = h.run("SELECT n FROM t", READ).expect("first");
    assert_eq!(first.cached, None);
    let second = h.run("SELECT n FROM t", READ).expect("second");
    assert!(second.cached.is_some(), "served from the cache");
    assert_eq!(second.sets, first.sets);
    assert_eq!(h.executions(), 1);

    // Refresh skips the cache but refills it.
    let refreshed = h
        .run(
            "SELECT n FROM t",
            ExecOptions {
                cache: CachePolicy::Refresh,
                page: None,
            },
        )
        .expect("refresh");
    assert_eq!(refreshed.cached, None);
    assert_eq!(h.executions(), 2);
}

#[test]
fn writes_and_outside_changes_invalidate() {
    let h = Harness::connected();
    h.run("SELECT n FROM t", READ).expect("read");

    // A write through Violet drops everything cached for the connection.
    h.run("UPDATE t SET n = 1", ExecOptions::WRITE)
        .expect("write");
    let after_write = h.run("SELECT n FROM t", READ).expect("read");
    assert_eq!(after_write.cached, None);

    // So does a change the server reports from elsewhere.
    h.server.token.fetch_add(1, Ordering::SeqCst);
    let after_change = h.run("SELECT n FROM t", READ).expect("read");
    assert_eq!(after_change.cached, None);
    assert_eq!(h.executions(), 4);

    // Reconnecting starts afresh.
    h.backend.connect(h.profile.clone(), None);
    h.wait(|event| matches!(event, Event::Connected { .. }));
    assert_eq!(h.run("SELECT n FROM t", READ).expect("read").cached, None);
}

#[test]
fn pages_are_read_one_at_a_time() {
    let h = Harness::connected();
    let page = |offset| ExecOptions {
        cache: CachePolicy::Use,
        page: Some(Page {
            offset,
            limit: Page::SIZE,
        }),
    };
    let first = h.run("SELECT n FROM t", page(0)).expect("first page");
    let info = first.page.expect("page info");
    assert!(info.has_more);
    assert_eq!(first.sets[0].rows.len(), 200);
    assert_eq!(first.sets[0].rows.get(0, 0), Some(Some("0")));
    // One extra row was asked for, to know more follow.
    assert!(h.server.executed.lock().expect("lock")[0].ends_with("LIMIT 201 OFFSET 0"));

    let last = h.run("SELECT n FROM t", page(400)).expect("last page");
    assert!(!last.page.expect("page info").has_more);
    assert_eq!(last.sets[0].rows.len(), 50);
    assert_eq!(last.sets[0].rows.get(0, 0), Some(Some("400")));

    // Each page is its own cache entry.
    assert!(
        h.run("SELECT n FROM t", page(0))
            .expect("again")
            .cached
            .is_some()
    );
}

#[test]
fn errors_point_at_the_users_text_and_counts_parse() {
    let h = Harness::connected();
    let paged = ExecOptions {
        cache: CachePolicy::Use,
        page: Some(Page::first()),
    };
    let error = h.run("SELECT fail", paged).expect_err("fails");
    // Position 20 of the wrapped text is 4 into the user's own.
    assert_eq!(error.position, Some(4));

    let request = h
        .backend
        .count(h.profile.id, None, "SELECT n FROM t".into());
    let counted =
        h.wait(|event| matches!(event, Event::Counted { request: r, .. } if *r == request));
    assert!(matches!(
        counted,
        Event::Counted {
            result: Ok(450),
            ..
        }
    ));
}

#[test]
fn a_missing_provider_is_installed_when_a_connection_needs_it() {
    let server = Arc::new(Server::default());
    let store = Arc::new(OnDemand {
        server: Arc::clone(&server),
        installs: AtomicU64::new(0),
    });
    let backend = Backend::start(
        Arc::clone(&store) as Arc<dyn ProviderStore>,
        Arc::new(MemorySecretStore::default()),
        Arc::new(MemoryResultCache::default()),
        || {},
    )
    .expect("runtime");
    assert!(
        backend.registry().infos().is_empty(),
        "nothing installed yet"
    );

    let mut profile = ConnectionProfile::new(DriverId::new("fake"), 1);
    profile.save_password = false;
    backend.connect(profile.clone(), None);
    let mut seen = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !seen.iter().any(|e| matches!(e, Event::Connected { .. })) {
        assert!(Instant::now() < deadline, "never connected: {seen:?}");
        seen.extend(backend.drain());
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(matches!(seen[0], Event::ProviderInstalling { .. }));
    assert!(matches!(
        seen[1],
        Event::ProviderInstalled { result: Ok(_), .. }
    ));
    assert_eq!(backend.registry().infos().len(), 1);

    // Installed once; the next connection reuses it.
    backend.connect(profile, None);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let events = backend.drain();
        if events.iter().any(|e| matches!(e, Event::Connected { .. })) {
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(store.installs.load(Ordering::SeqCst), 1);

    // A provider nobody offers fails the connection with the reason.
    backend.connect(ConnectionProfile::new(DriverId::new("nope"), 1), None);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let failed = backend.drain().into_iter().find_map(|e| match e {
            Event::ConnectFailed { error, .. } => Some(error),
            _ => None,
        });
        if let Some(error) = failed {
            assert_eq!(error.kind, ErrorKind::Provider);
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// A key-value store: it pages, counts and lists itself, so Violet must
/// never send it SQL of its own.
struct KeyValueDriver(Arc<Server>);
struct KeyValueSession(Arc<Server>);

#[async_trait]
impl Driver for KeyValueDriver {
    fn info(&self) -> DriverInfo {
        DriverInfo {
            id: DriverId::new("kv"),
            language: violet_core::Language::Redis,
            user: violet_core::Login::Optional,
            ..FakeDriver(Arc::clone(&self.0)).info()
        }
    }

    async fn connect(&self, _: &ConnectionProfile, _: &Credentials) -> Result<Box<dyn Session>> {
        Ok(Box::new(KeyValueSession(Arc::clone(&self.0))))
    }
}

impl KeyValueSession {
    fn saw(&self, what: String) {
        self.0.executed.lock().expect("lock").push(what);
    }
}

#[async_trait]
impl Session for KeyValueSession {
    async fn server_info(&self) -> Result<ServerInfo> {
        Ok(ServerInfo {
            version: "KV 1".into(),
            database: "db0".into(),
            user: "default".into(),
        })
    }
    async fn databases(&self) -> Result<Vec<String>> {
        Ok(Vec::new())
    }
    async fn schemas(&self) -> Result<Vec<String>> {
        Ok(vec!["db0".into(), "db1".into()])
    }
    async fn objects(&self, _: &str) -> Result<Vec<DbObject>> {
        Ok(Vec::new())
    }
    async fn columns(&self, _: &str, _: &str) -> Result<Vec<ColumnInfo>> {
        Ok(Vec::new())
    }
    async fn execute(&self, sql: &str, _: ExecLimits) -> Result<Vec<ResultSet>> {
        self.saw(format!("execute {sql}"));
        Ok(Vec::new())
    }
    async fn page(
        &self,
        schema: &str,
        statement: &str,
        offset: u64,
        limit: u32,
    ) -> Result<Vec<ResultSet>> {
        self.saw(format!("page {schema} {statement} {offset} {limit}"));
        let mut set = ResultSet {
            columns: vec![column("member")],
            ..ResultSet::default()
        };
        for n in offset..(offset + u64::from(limit) + 1).min(250) {
            set.rows.push([Some(n.to_string().as_str())]);
        }
        Ok(vec![set])
    }
    async fn count(&self, schema: &str, statement: &str) -> Result<Option<u64>> {
        self.saw(format!("count {schema} {statement}"));
        Ok(Some(250))
    }
    async fn run_in(
        &self,
        schema: &str,
        statements: &str,
        _: ExecLimits,
    ) -> Result<Vec<ResultSet>> {
        self.saw(format!("run {schema} {statements}"));
        Ok(Vec::new())
    }
    fn canceller(&self) -> Option<Box<dyn Canceller>> {
        None
    }
    fn dialect(&self) -> &dyn Dialect {
        &AnsiDialect
    }
    fn language(&self) -> violet_core::Language {
        violet_core::Language::Redis
    }
    async fn close(&self) {}
}

#[test]
fn drivers_that_are_not_sql_never_get_sql() {
    let server = Arc::new(Server::default());
    let registry = DriverRegistry::new().with(KeyValueDriver(Arc::clone(&server)));
    let backend = Backend::with_registry(
        registry,
        Arc::new(OnDemand {
            server: Arc::clone(&server),
            installs: AtomicU64::new(0),
        }),
        Arc::new(MemorySecretStore::default()),
        Arc::new(MemoryResultCache::default()),
        || {},
    )
    .expect("runtime");
    let mut profile = ConnectionProfile::new(DriverId::new("kv"), 1);
    profile.save_password = false;
    backend.connect(profile.clone(), None);
    let h = Harness {
        backend,
        server,
        profile,
    };
    h.wait(|event| matches!(event, Event::Connected { .. }));
    let id = h.profile.id;

    assert_eq!(h.backend.language(id), Some(violet_core::Language::Redis));
    assert_eq!(h.backend.explain_sql(id, "GET k", false), None);
    let browse = h
        .backend
        .browse_sql(id, "db1", "k", None, None, Some("set"))
        .expect("browse");
    assert_eq!(browse, "SMEMBERS k");

    // A data view pages in its database, twice: once from the cache would
    // be wrong, there is no change token to trust.
    for _ in 0..2 {
        let request = h.backend.execute_in(
            id,
            Some("db1".into()),
            browse.clone(),
            ExecOptions {
                cache: CachePolicy::Use,
                page: Some(Page::first()),
            },
        );
        let Event::QueryFinished { result, .. } = h.wait(
            |event| matches!(event, Event::QueryFinished { request: r, .. } if *r == request),
        ) else {
            unreachable!()
        };
        let output = result.expect("page");
        let page = output.page.expect("paged");
        assert!(page.has_more);
        assert_eq!(output.sets[0].rows.len(), 200);
    }
    let request = h.backend.count(id, Some("db1".into()), browse.clone());
    let counted =
        h.wait(|event| matches!(event, Event::Counted { request: r, .. } if *r == request));
    assert!(matches!(
        counted,
        Event::Counted {
            result: Ok(250),
            ..
        }
    ));
    // The tree's commands run in their database; the editor's as typed.
    let request = h.backend.execute_in(
        id,
        Some("db1".into()),
        "UNLINK k".into(),
        ExecOptions::WRITE,
    );
    h.wait(|event| matches!(event, Event::QueryFinished { request: r, .. } if *r == request));
    let request = h.backend.execute(id, "GET k".into(), ExecOptions::WRITE);
    h.wait(|event| matches!(event, Event::QueryFinished { request: r, .. } if *r == request));
    h.backend.load_foreign_keys(id, "db1".into());
    h.wait(|event| matches!(event, Event::ForeignKeys { result: Ok(keys), .. } if keys.is_empty()));

    let seen = h.server.executed.lock().expect("lock").clone();
    assert_eq!(
        seen,
        [
            "page db1 SMEMBERS k 0 200",
            "page db1 SMEMBERS k 0 200",
            "count db1 SMEMBERS k",
            "run db1 UNLINK k",
            "execute GET k",
        ]
    );
}
