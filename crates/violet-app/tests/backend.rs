//! The backend against a scripted driver: the result cache, paging,
//! transactions, and providers installed on demand.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use violet_app::{
    Backend, CachePolicy, DriverRegistry, Event, ExecOptions, Transaction, TxCommand, TxControl,
};
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
    /// A statement failed inside the transaction: `COMMIT` rolls back, as
    /// PostgreSQL's does.
    aborted: std::sync::atomic::AtomicBool,
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
        if sql == "COMMIT" && self.0.aborted.swap(false, Ordering::SeqCst) {
            return Ok(vec![ResultSet {
                command: Some("ROLLBACK".into()),
                ..ResultSet::default()
            }]);
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
    /// The transaction as last reported.
    transaction: std::cell::Cell<Transaction>,
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
            transaction: std::cell::Cell::default(),
        };
        harness.wait(|event| matches!(event, Event::Connected { .. }));
        harness
    }

    fn wait(&self, mut wanted: impl FnMut(&Event) -> bool) -> Event {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            for event in self.backend.drain() {
                if let Event::Transaction { state, .. } = &event {
                    self.transaction.set(*state);
                }
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

    /// What was sent since the last call.
    fn sent(&self) -> Vec<String> {
        std::mem::take(&mut *self.server.executed.lock().expect("lock"))
    }

    fn script(&self, sql: &str, control: TxControl) -> Result<QueryOutput> {
        let request = self.backend.execute_script(
            self.profile.id,
            sql.to_owned(),
            ExecOptions::WRITE,
            control,
        );
        match self
            .wait(|event| matches!(event, Event::QueryFinished { request: r, .. } if *r == request))
        {
            Event::QueryFinished { result, .. } => result,
            _ => unreachable!(),
        }
    }

    fn transact(&self, command: TxCommand) -> Result<()> {
        self.backend.transaction(self.profile.id, command);
        match self.wait(|event| matches!(event, Event::TransactionDone { .. })) {
            Event::TransactionDone { result, .. } => result,
            _ => unreachable!(),
        }
    }

    /// The transaction as reported by the time the last run or command
    /// finished.
    fn state(&self) -> Transaction {
        self.transaction.get()
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

    let request = h.backend.count(h.profile.id, "SELECT n FROM t".into());
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

#[test]
fn manual_commit_opens_a_transaction_before_the_first_write() {
    let h = Harness::connected();
    h.transact(TxCommand::AutoCommit(false)).expect("manual");
    assert!(h.state().manual);

    // Reads open nothing.
    h.run("SELECT n FROM t", READ).expect("read");
    assert_eq!(h.sent(), ["SELECT n FROM t"]);

    h.run("UPDATE t SET n = 1", ExecOptions::WRITE)
        .expect("write");
    assert_eq!(h.sent(), ["BEGIN", "UPDATE t SET n = 1"]);
    let state = h.state();
    assert!(state.open);
    assert_eq!(state.writes, 1);
    h.run("DELETE FROM t", ExecOptions::WRITE).expect("write");
    assert_eq!(h.sent(), ["DELETE FROM t"]);
    assert_eq!(h.state().writes, 2);

    // Inside it nothing comes from the cache, or goes into it.
    h.run("SELECT n FROM t", READ).expect("read");
    let again = h.run("SELECT n FROM t", READ).expect("read");
    assert_eq!(again.cached, None);
    assert_eq!(h.sent(), ["SELECT n FROM t", "SELECT n FROM t"]);

    h.transact(TxCommand::Commit).expect("commit");
    assert_eq!(h.sent(), ["COMMIT"]);
    assert_eq!(
        h.state(),
        Transaction {
            manual: true,
            open: false,
            writes: 0
        }
    );

    // The next write opens the next transaction; turning auto-commit back
    // on commits it.
    h.run("UPDATE t SET n = 2", ExecOptions::WRITE)
        .expect("write");
    assert_eq!(h.sent(), ["BEGIN", "UPDATE t SET n = 2"]);
    h.transact(TxCommand::AutoCommit(true))
        .expect("auto-commit");
    assert_eq!(h.sent(), ["COMMIT"]);
    h.run("UPDATE t SET n = 3", ExecOptions::WRITE)
        .expect("write");
    assert_eq!(h.sent(), ["UPDATE t SET n = 3"]);
}

#[test]
fn rollbacks_and_reconnects_close_the_transaction() {
    let h = Harness::connected();
    h.transact(TxCommand::AutoCommit(false)).expect("manual");
    h.run("UPDATE t SET n = 1", ExecOptions::WRITE)
        .expect("write");
    h.transact(TxCommand::Rollback).expect("rollback");
    assert_eq!(h.sent(), ["BEGIN", "UPDATE t SET n = 1", "ROLLBACK"]);

    h.run("UPDATE t SET n = 1", ExecOptions::WRITE)
        .expect("write");
    h.backend.connect(h.profile.clone(), None);
    h.wait(|event| matches!(event, Event::Connected { .. }));
    h.sent();
    // The new session is still in manual commit, with nothing open.
    h.run("UPDATE t SET n = 2", ExecOptions::WRITE)
        .expect("write");
    assert_eq!(h.sent(), ["BEGIN", "UPDATE t SET n = 2"]);
}

#[test]
fn scripts_that_open_or_end_their_own_transaction_are_followed() {
    let h = Harness::connected();
    let begin = TxControl {
        begins: true,
        open_after: Some(true),
    };
    let end = TxControl {
        begins: false,
        open_after: Some(false),
    };
    // In auto-commit, a typed BEGIN still shows as an open transaction.
    h.script("BEGIN", begin).expect("begin");
    assert!(h.state().open);
    h.script("COMMIT", end).expect("commit");
    assert!(!h.state().open);

    // In manual commit Violet adds no BEGIN of its own to one.
    h.transact(TxCommand::AutoCommit(false)).expect("manual");
    h.sent();
    h.script("BEGIN; UPDATE t SET n = 1", begin).expect("begin");
    assert_eq!(h.sent(), ["BEGIN; UPDATE t SET n = 1"]);
    h.script("COMMIT", end).expect("commit");

    // A script that opened one and failed may have left it open.
    let control = TxControl {
        begins: true,
        open_after: Some(false),
    };
    h.script("BEGIN; fail; COMMIT", control).expect_err("fails");
    assert!(h.state().open);
}

#[test]
fn a_commit_the_server_turns_into_a_rollback_is_an_error() {
    let h = Harness::connected();
    h.transact(TxCommand::AutoCommit(false)).expect("manual");
    h.run("UPDATE t SET n = 1", ExecOptions::WRITE)
        .expect("write");
    h.server
        .aborted
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let error = h.transact(TxCommand::Commit).expect_err("rolled back");
    assert!(error.message.contains("rolled it back"));
    assert!(!h.state().open);
}
