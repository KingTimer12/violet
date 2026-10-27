//! Running database work off the interface thread.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use tokio::runtime::Runtime;
use tokio::sync::watch;
use violet_core::ports::{
    CacheKey, CachedOutput, ProviderListing, ProviderStore, ResultCache, SecretStore,
};
use violet_core::{
    Canceller, ColumnInfo, ConnectionId, ConnectionProfile, Credentials, DbError, DbObject, Driver,
    DriverId, DriverInfo, EntryPage, ErrorKind, ExecLimits, ForeignKey, Language, ManualCommit,
    OrderBy, Page, PageInfo, QueryOutput, Result, ResultSet, Secret, ServerInfo, Session, Task,
    TaskKind, TaskRequest, TaskSettings, ToolCommand,
};

use crate::registry::DriverRegistry;
use crate::tasks::{self, Program, TaskSummary};

/// Wakes the interface when events are waiting. The egui app implements it
/// with `Context::request_repaint`.
pub trait Notify: Send + Sync + 'static {
    fn notify(&self);
}

impl<F: Fn() + Send + Sync + 'static> Notify for F {
    fn notify(&self) {
        self();
    }
}

/// Whether a run may use the result cache.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CachePolicy {
    /// A read-only query: answer from the cache when nothing changed since,
    /// else run it and keep the result.
    Use,
    /// A read-only query the user asked to refresh: run it and keep the
    /// result.
    Refresh,
    /// Anything else. It runs, and every result cached for the connection
    /// is dropped afterwards, since it may have written.
    Off,
}

/// How to run a query.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExecOptions {
    pub cache: CachePolicy,
    /// Read only this window of the rows. The query must be a single
    /// read-only statement.
    pub page: Option<Page>,
}

impl ExecOptions {
    /// A statement that may write: no cache, no paging.
    pub const WRITE: Self = Self {
        cache: CachePolicy::Off,
        page: None,
    };
}

/// A connection's transaction, as the backend keeps track of it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Transaction {
    /// Writes wait for a commit; off by default, when each statement
    /// commits on its own.
    pub manual: bool,
    /// A transaction is open: what it wrote waits for a commit or rollback.
    pub open: bool,
    /// Runs that may have written since it opened.
    pub writes: u32,
}

/// What a script's own transaction statements do, as the interface reads
/// them: Violet then neither opens one for it nor loses track.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TxControl {
    /// A statement opens a transaction: `BEGIN`, `START TRANSACTION`.
    pub begins: bool,
    /// Whether one is open after the last statement that opens or ends one
    /// (`COMMIT`, `ROLLBACK`, `END`); `None` without such statements.
    pub open_after: Option<bool>,
}

/// What the user asks of a connection's transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TxCommand {
    Commit,
    Rollback,
    /// Turns auto-commit on (`true`) or off. Turning it on commits an open
    /// transaction first, as JDBC does.
    AutoCommit(bool),
}

/// Identifies one query or connection test, to match its result and to
/// cancel it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RequestId(u64);

/// What a driver offers for backup and restore.
#[derive(Clone, Debug)]
pub struct Offer {
    pub tasks: Vec<Task>,
    /// The client programs it downloads for this machine.
    pub download: Option<violet_core::Download>,
}

/// Identifies one running backup or restore.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TaskId(u64);

/// What the backend reports.
#[derive(Debug)]
pub enum Event {
    Connected {
        id: ConnectionId,
        server: ServerInfo,
    },
    ConnectFailed {
        id: ConnectionId,
        error: DbError,
    },
    Disconnected {
        id: ConnectionId,
    },
    /// The other databases on the session's server.
    Databases {
        id: ConnectionId,
        result: Result<Vec<String>>,
    },
    Schemas {
        id: ConnectionId,
        result: Result<Vec<String>>,
    },
    Objects {
        id: ConnectionId,
        schema: String,
        result: Result<Vec<DbObject>>,
    },
    Columns {
        id: ConnectionId,
        schema: String,
        relation: String,
        result: Result<Vec<ColumnInfo>>,
    },
    /// A page of a schema's keys, for drivers that list them so.
    Entries {
        id: ConnectionId,
        schema: String,
        /// The glob they match.
        pattern: String,
        /// They follow the ones listed before, rather than replace them.
        more: bool,
        result: Result<EntryPage>,
    },
    /// A schema's keys starting with `prefix`, looked up for completion.
    KeysMatching {
        id: ConnectionId,
        schema: String,
        prefix: String,
        result: Result<EntryPage>,
    },
    /// Foreign keys from or to a schema's tables.
    ForeignKeys {
        id: ConnectionId,
        schema: String,
        result: Result<Vec<ForeignKey>>,
    },
    QueryFinished {
        request: RequestId,
        result: Result<QueryOutput>,
    },
    Counted {
        request: RequestId,
        result: Result<u64>,
    },
    TestFinished {
        request: RequestId,
        result: Result<ServerInfo>,
    },
    /// The provider catalogue, with what is installed.
    Providers(Result<Vec<ProviderListing>>),
    /// A connection needs a provider that is not installed; it is being
    /// downloaded.
    ProviderInstalling {
        id: DriverId,
    },
    ProviderInstalled {
        id: DriverId,
        result: Result<DriverInfo>,
    },
    ProviderRemoved {
        id: DriverId,
        result: Result<()>,
    },
    /// The backups and restores a driver offers.
    Tasks {
        driver: DriverId,
        result: Result<Offer>,
    },
    /// Where a task's programs are on this machine.
    ProgramsFound {
        driver: DriverId,
        kind: TaskKind,
        programs: Vec<Program>,
    },
    /// The command a task would run, to show before it does.
    TaskPreview {
        request: RequestId,
        result: Result<(PathBuf, ToolCommand)>,
    },
    /// Settings that suit restoring from a path.
    InputInspected {
        path: PathBuf,
        result: Result<TaskSettings>,
    },
    /// A provider's client programs are being downloaded.
    ToolsDownloading {
        driver: DriverId,
        label: String,
        size: u64,
    },
    /// They are in (`None` when none is offered for this machine), or the
    /// download failed.
    ToolsReady {
        driver: DriverId,
        result: Result<Option<PathBuf>>,
    },
    /// Lines a running task's program printed.
    TaskOutput {
        task: TaskId,
        lines: Vec<String>,
    },
    TaskFinished {
        task: TaskId,
        result: Result<TaskSummary>,
    },
    /// A connection's transaction changed: its mode, whether one is open,
    /// or what it wrote.
    Transaction {
        id: ConnectionId,
        state: Transaction,
    },
    /// A commit, rollback or change of mode finished.
    TransactionDone {
        id: ConnectionId,
        command: TxCommand,
        result: Result<()>,
    },
}

type Sessions = HashMap<ConnectionId, Arc<dyn Session>>;
/// Held while a run or a commit uses the transaction, so a `BEGIN` and the
/// statement it opens for go together.
type Transactions = HashMap<ConnectionId, Arc<tokio::sync::Mutex<Transaction>>>;
type Running = HashMap<RequestId, Arc<dyn Canceller>>;
type RunningTasks = HashMap<TaskId, watch::Sender<bool>>;

/// Shared between the backend handle and the tasks it spawns.
struct Shared {
    registry: DriverRegistry,
    providers: Arc<dyn ProviderStore>,
    secrets: Arc<dyn SecretStore>,
    cache: Arc<dyn ResultCache>,
    sessions: Mutex<Sessions>,
    transactions: Mutex<Transactions>,
    running: Mutex<Running>,
    tasks: Mutex<RunningTasks>,
    events: Mutex<Sender<Event>>,
    notify: Box<dyn Notify>,
}

impl Shared {
    fn sessions(&self) -> MutexGuard<'_, Sessions> {
        self.sessions.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// `id`'s transaction, auto-commit until told otherwise.
    fn transaction(&self, id: ConnectionId) -> Arc<tokio::sync::Mutex<Transaction>> {
        let mut transactions = self
            .transactions
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        Arc::clone(transactions.entry(id).or_default())
    }

    /// Reports `id`'s transaction when it changed from `before`.
    fn report(&self, id: ConnectionId, before: Transaction, state: Transaction) {
        if state != before {
            self.emit(Event::Transaction { id, state });
        }
    }

    /// Forgets the transaction a session that is gone had open; a new
    /// session in manual commit leaves auto-commit as the old one did.
    async fn reset_transaction(&self, id: ConnectionId, session: Option<&Arc<dyn Session>>) {
        let transaction = self.transaction(id);
        let mut tx = transaction.lock().await;
        let before = *tx;
        tx.open = false;
        tx.writes = 0;
        if tx.manual
            && let Some(session) = session.filter(|session| session.language() == Language::Sql)
            && let ManualCommit::Setting { off, .. } = session.dialect().manual_commit()
            && let Err(error) = session.execute(off, ExecLimits::default()).await
        {
            log::warn!("manual commit not set on the new session: {error}");
            tx.manual = false;
        }
        self.report(id, before, *tx);
    }

    /// Runs `command` on `id`'s transaction.
    async fn transact(
        &self,
        id: ConnectionId,
        command: TxCommand,
        limits: ExecLimits,
    ) -> Result<()> {
        let transaction = self.transaction(id);
        let mut tx = transaction.lock().await;
        let before = *tx;
        let session = self.session(id).ok();
        if session
            .as_ref()
            .is_some_and(|session| session.language() != Language::Sql)
        {
            return Err(DbError::unsupported("This database has no transactions"));
        }
        let result = async {
            let ending = match command {
                TxCommand::Commit => Some(true),
                TxCommand::Rollback => Some(false),
                TxCommand::AutoCommit(on) => (on && tx.open).then_some(true),
            };
            if let Some(commit) = ending
                && let Some(session) = &session
            {
                let sql = if commit { "COMMIT" } else { "ROLLBACK" };
                let result = session.execute(sql, limits).await.and_then(|sets| {
                    // PostgreSQL ends a failed transaction this way.
                    let rolled_back = sets
                        .iter()
                        .any(|set| set.command.as_deref() == Some("ROLLBACK"));
                    if commit && rolled_back {
                        tx.open = false;
                        tx.writes = 0;
                        self.cache.invalidate(id);
                        return Err(DbError::new(
                            ErrorKind::Query,
                            "A statement in the transaction had failed, so the server rolled \
                             it back instead of committing",
                        ));
                    }
                    Ok(())
                });
                // A failed commit may leave the transaction open (a busy
                // SQLite file); a failed rollback leaves nothing to undo.
                if result.is_ok() || !commit {
                    tx.open = false;
                    tx.writes = 0;
                    // What was read inside it may be gone, or now shared.
                    self.cache.invalidate(id);
                }
                result?;
            } else if ending.is_some() {
                tx.open = false;
                tx.writes = 0;
            }
            if let TxCommand::AutoCommit(on) = command
                && tx.manual == on
            {
                // Without a session the mode waits for the next one.
                if let Some(session) = &session
                    && let ManualCommit::Setting { off, on: back } =
                        session.dialect().manual_commit()
                {
                    session.execute(if on { back } else { off }, limits).await?;
                }
                tx.manual = !on;
            }
            Ok(())
        }
        .await;
        self.report(id, before, *tx);
        result
    }

    fn running(&self) -> MutexGuard<'_, Running> {
        self.running.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn tasks(&self) -> MutexGuard<'_, RunningTasks> {
        self.tasks.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The driver's task of `kind`.
    async fn task(&self, driver: &Arc<dyn Driver>, kind: TaskKind) -> Result<Task> {
        driver
            .tasks()
            .await?
            .into_iter()
            .find(|task| task.kind == kind)
            .ok_or_else(|| DbError::unsupported(format!("No {kind} for this database")))
    }

    /// The program and command for `request`, checked and found.
    async fn task_command(
        self: &Arc<Self>,
        profile: &ConnectionProfile,
        request: &TaskRequest,
        folder: Option<&Path>,
    ) -> Result<(PathBuf, ToolCommand)> {
        let driver = self.driver(&profile.driver).await?;
        let task = self.task(&driver, request.kind).await?;
        // Violet fills the provider's template: the connection comes from
        // the profile, whatever the provider would like.
        let command = task.build(profile, request)?;
        let downloaded = self.providers.tools(&profile.driver);
        let folders = tasks::search_folders(&task, folder, downloaded.as_deref());
        let program = tasks::find(&command.program, &folders).ok_or_else(|| {
            DbError::new(
                ErrorKind::Other,
                format!(
                    "{} was not found. Violet downloads it with the provider where it can; \
                     otherwise install the database's client tools, or pick the folder \
                     they are in.",
                    command.program
                ),
            )
        })?;
        Ok((program, command))
    }

    fn session(&self, id: ConnectionId) -> Result<Arc<dyn Session>> {
        self.sessions()
            .get(&id)
            .cloned()
            .ok_or_else(DbError::not_connected)
    }

    fn emit(&self, event: Event) {
        let sent = self
            .events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .send(event);
        // The receiver lives as long as the backend; a failed send means
        // the app is shutting down.
        if sent.is_ok() {
            self.notify.notify();
        }
    }

    /// The password to open `profile` with: the one typed, else the
    /// keychain's. Keychain calls can block, so they run off the runtime's
    /// worker threads.
    async fn credentials(
        self: &Arc<Self>,
        profile: &ConnectionProfile,
        typed: Option<Secret>,
    ) -> Result<Credentials> {
        if typed.is_some() || !profile.save_password {
            return Ok(Credentials { password: typed });
        }
        let shared = Arc::clone(self);
        let id = profile.id;
        let password = tokio::task::spawn_blocking(move || shared.secrets.get(id))
            .await
            .map_err(|error| DbError::new(ErrorKind::Other, error.to_string()))??;
        Ok(Credentials { password })
    }

    /// Downloads the client programs a driver's provider asks for, in the
    /// background, unless they are in already.
    fn ensure_tools(self: &Arc<Self>, id: DriverId, driver: Arc<dyn Driver>) {
        let shared = Arc::clone(self);
        tokio::spawn(async move {
            let downloads = match driver.downloads().await {
                Ok(downloads) => downloads,
                Err(error) => {
                    shared.emit(Event::ToolsReady {
                        driver: id,
                        result: Err(error),
                    });
                    return;
                }
            };
            let Some(download) = violet_core::Download::this_platform()
                .and_then(|platform| downloads.iter().find(|d| d.platform == platform))
            else {
                return;
            };
            let have = shared.providers.tools(&id).is_some();
            if !have {
                shared.emit(Event::ToolsDownloading {
                    driver: id.clone(),
                    label: download.label.clone(),
                    size: download.size,
                });
            }
            let result = shared.providers.install_tools(&id, downloads).await;
            if !have || result.is_err() {
                shared.emit(Event::ToolsReady { driver: id, result });
            }
        });
    }

    /// The driver for `id`, installing its provider first if needed.
    #[hotpath::measure]
    async fn driver(self: &Arc<Self>, id: &DriverId) -> Result<Arc<dyn Driver>> {
        if let Some(driver) = self.registry.get(id) {
            return Ok(driver);
        }
        self.emit(Event::ProviderInstalling { id: id.clone() });
        let result = self.providers.install(id).await;
        if let Ok(driver) = &result {
            self.registry.add(Arc::clone(driver));
            // Its client programs follow, without holding the connection.
            self.ensure_tools(id.clone(), Arc::clone(driver));
        }
        self.emit(Event::ProviderInstalled {
            id: id.clone(),
            result: result
                .as_ref()
                .map(|driver| driver.info())
                .map_err(Clone::clone),
        });
        result
    }

    #[hotpath::measure]
    async fn open(
        self: &Arc<Self>,
        profile: &ConnectionProfile,
        typed: Option<Secret>,
    ) -> Result<(Box<dyn Session>, ServerInfo)> {
        let driver = self.driver(&profile.driver).await?;
        let credentials = self.credentials(profile, typed).await?;
        let session = driver.connect(profile, &credentials).await?;
        let server = session.server_info().await?;
        Ok((session, server))
    }

    /// Runs `sql` on `id`'s session as `options` say, through the cache
    /// and inside the connection's transaction. `schema`, for drivers that
    /// are not SQL, is where it runs; SQL names its schemas itself.
    #[hotpath::measure]
    #[allow(clippy::too_many_arguments)]
    async fn query(
        &self,
        request: RequestId,
        id: ConnectionId,
        sql: String,
        options: ExecOptions,
        limits: ExecLimits,
        schema: Option<String>,
        control: TxControl,
    ) -> Result<QueryOutput> {
        let session = self.session(id)?;
        // Transactions are SQL's: other drivers run as they are.
        if session.language() != Language::Sql {
            return Self::query_other(&*session, sql, options, limits, schema).await;
        }
        let transaction = self.transaction(id);
        let mut tx = transaction.lock().await;
        let before = *tx;
        let writes = options.cache == CachePolicy::Off;
        // Manual commit opens a transaction before the first write; a
        // script that opens or ends its own is left to it.
        if writes && tx.manual && !tx.open && control.open_after.is_none() {
            if let ManualCommit::Begin(begin) = session.dialect().manual_commit() {
                session.execute(begin, limits).await?;
            }
            tx.open = true;
        }
        let result = self
            .query_in(request, id, &session, sql, options, limits, tx.open)
            .await;
        match (&result, control.open_after) {
            (Ok(_), Some(open)) => {
                if tx.open && !open {
                    self.cache.invalidate(id);
                }
                tx.open = open;
                tx.writes = 0;
            }
            // Where a failing script stopped is unknown: had it opened a
            // transaction, one may well be open.
            (Err(_), _) if control.begins => tx.open = true,
            _ => {}
        }
        if result.is_ok() && writes && tx.open && control.open_after.is_none() {
            tx.writes = tx.writes.saturating_add(1);
        }
        self.report(id, before, *tx);
        result
    }

    /// [`Shared::query`] once the transaction is settled. Inside an open
    /// one the cache is left alone: its rows may be rolled back, and a
    /// change token may not move until it ends.
    #[allow(clippy::too_many_arguments)]
    async fn query_in(
        &self,
        request: RequestId,
        id: ConnectionId,
        session: &Arc<dyn Session>,
        sql: String,
        options: ExecOptions,
        limits: ExecLimits,
        in_transaction: bool,
    ) -> Result<QueryOutput> {
        // A page may ask for more than the usual cap ("fetch all"); its own
        // limit bounds it.
        let limits = options.page.map_or(limits, |page| ExecLimits {
            max_rows: limits
                .max_rows
                .max(usize::try_from(page.limit.saturating_add(1)).unwrap_or(usize::MAX)),
        });
        let (sent, shift) = match options.page {
            Some(page) => {
                // One row more than asked tells whether another page follows.
                let paged =
                    session
                        .dialect()
                        .paged(&sql, page.limit.saturating_add(1), page.offset);
                (paged.sql, paged.shift)
            }
            None => (sql, 0),
        };
        let writes = options.cache == CachePolicy::Off;
        let cacheable = !writes && !in_transaction;
        let key = CacheKey {
            connection: id,
            sql: sent,
        };

        // The token is read before the query, so a change racing it makes
        // the stored token stale rather than the stored rows.
        let token = if cacheable {
            session.change_token().await.unwrap_or_else(|error| {
                log::debug!("change token unavailable: {error}");
                None
            })
        } else {
            None
        };
        if options.cache == CachePolicy::Use
            && let Some(hit) = self.cache.get(&key)
            && hit.token == token
        {
            let mut output = (*hit.output).clone();
            output.cached = Some(hit.stored.elapsed());
            return Ok(output);
        }

        if let Some(canceller) = session.canceller() {
            self.running().insert(request, Arc::from(canceller));
        }
        let started = Instant::now();
        let result = session.execute(&key.sql, limits).await;
        self.running().remove(&request);
        if writes {
            self.cache.invalidate(id);
        }
        let mut sets = result.map_err(|mut error| {
            // Point at the user's text, not the paging wrapper around it.
            error.position = error
                .position
                .and_then(|at| at.checked_sub(shift))
                .filter(|at| *at > 0);
            error
        })?;

        let page = options.page.map(|page| {
            let limit = usize::try_from(page.limit).unwrap_or(usize::MAX);
            let has_more = sets.first().is_some_and(|set| set.rows.len() > limit);
            if let Some(set) = sets.first_mut() {
                set.rows.truncate(limit);
                set.truncated = false;
            }
            PageInfo {
                offset: page.offset,
                limit: page.limit,
                has_more,
            }
        });
        let output = QueryOutput {
            sets,
            elapsed: started.elapsed(),
            page,
            cached: None,
        };
        if cacheable {
            self.cache.put(
                key,
                CachedOutput {
                    output: Arc::new(output.clone()),
                    token,
                    stored: Instant::now(),
                },
            );
        }
        Ok(output)
    }

    /// [`Shared::query`] for a driver that is not SQL: it pages itself,
    /// and nothing is cached (it has no change token to check against).
    async fn query_other(
        session: &dyn Session,
        sql: String,
        options: ExecOptions,
        limits: ExecLimits,
        schema: Option<String>,
    ) -> Result<QueryOutput> {
        let started = Instant::now();
        let (sets, page) = match (options.page, schema) {
            (Some(page), Some(schema)) => {
                let limit = u32::try_from(page.limit).unwrap_or(u32::MAX);
                let mut sets = session.page(&schema, &sql, page.offset, limit).await?;
                let limit = usize::try_from(page.limit).unwrap_or(usize::MAX);
                let has_more = sets.first().is_some_and(|set| set.rows.len() > limit);
                if let Some(set) = sets.first_mut() {
                    set.rows.truncate(limit);
                    set.truncated = false;
                }
                let info = PageInfo {
                    offset: page.offset,
                    limit: page.limit,
                    has_more,
                };
                (sets, Some(info))
            }
            (None, Some(schema)) => (session.run_in(&schema, &sql, limits).await?, None),
            (_, None) => (session.execute(&sql, limits).await?, None),
        };
        Ok(QueryOutput {
            sets,
            elapsed: started.elapsed(),
            page,
            cached: None,
        })
    }
}

/// The interface's handle on everything that talks to databases.
pub struct Backend {
    runtime: Runtime,
    shared: Arc<Shared>,
    events: Receiver<Event>,
    next_request: AtomicU64,
    limits: ExecLimits,
}

impl Backend {
    /// Starts the runtime with the installed providers registered.
    /// `notify` is called from worker threads whenever an event is queued.
    pub fn start(
        providers: Arc<dyn ProviderStore>,
        secrets: Arc<dyn SecretStore>,
        cache: Arc<dyn ResultCache>,
        notify: impl Notify,
    ) -> std::io::Result<Self> {
        let registry = DriverRegistry::new();
        match providers.installed() {
            Ok(drivers) => {
                for driver in drivers {
                    registry.add(driver);
                }
            }
            Err(error) => log::error!("installed providers: {error}"),
        }
        Self::with_registry(registry, providers, secrets, cache, notify)
    }

    /// Starts the runtime with `registry` as it is, as tests do.
    pub fn with_registry(
        registry: DriverRegistry,
        providers: Arc<dyn ProviderStore>,
        secrets: Arc<dyn SecretStore>,
        cache: Arc<dyn ResultCache>,
        notify: impl Notify,
    ) -> std::io::Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("violet-db")
            .enable_all()
            .build()?;
        let (sender, events) = mpsc::channel();
        Ok(Self {
            runtime,
            shared: Arc::new(Shared {
                registry,
                providers,
                secrets,
                cache,
                sessions: Mutex::default(),
                transactions: Mutex::default(),
                running: Mutex::default(),
                tasks: Mutex::default(),
                events: Mutex::new(sender),
                notify: Box::new(notify),
            }),
            events,
            next_request: AtomicU64::new(1),
            limits: ExecLimits::default(),
        })
    }

    #[must_use]
    pub fn registry(&self) -> &DriverRegistry {
        &self.shared.registry
    }

    /// Events queued since the last call, oldest first.
    pub fn drain(&self) -> Vec<Event> {
        self.events.try_iter().collect()
    }

    #[must_use]
    pub fn is_connected(&self, id: ConnectionId) -> bool {
        self.shared.sessions().contains_key(&id)
    }

    /// How a data view reads a relation, in the session's dialect, or a
    /// key of type `subtype` for a key-value store.
    #[must_use]
    pub fn browse_sql(
        &self,
        id: ConnectionId,
        schema: &str,
        relation: &str,
        filter: Option<&str>,
        order: Option<&OrderBy>,
        subtype: Option<&str>,
    ) -> Option<String> {
        let session = self.shared.session(id).ok()?;
        Some(match session.language() {
            Language::Redis => violet_lang::redis::browse(subtype, relation, filter),
            _ => session.dialect().browse(schema, relation, filter, order),
        })
    }

    /// How the session's dialect explains `sql`; `None` for languages
    /// Violet has no plan for.
    #[must_use]
    pub fn explain_sql(&self, id: ConnectionId, sql: &str, analyze: bool) -> Option<String> {
        let session = self.shared.session(id).ok()?;
        (session.language() == Language::Sql).then(|| session.dialect().explain(sql, analyze))
    }

    /// What the open session's statements are written in.
    #[must_use]
    pub fn language(&self, id: ConnectionId) -> Option<Language> {
        Some(self.shared.session(id).ok()?.language())
    }

    /// The most rows one result may hold.
    #[must_use]
    pub fn max_rows(&self) -> usize {
        self.limits.max_rows
    }

    fn request(&self) -> RequestId {
        RequestId(self.next_request.fetch_add(1, Ordering::Relaxed))
    }

    /// Asks a driver for its backup and restore tasks, installing its
    /// provider if needed.
    pub fn load_tasks(&self, driver: DriverId) {
        let shared = Arc::clone(&self.shared);
        self.runtime.spawn(async move {
            let result = async {
                let found = shared.driver(&driver).await?;
                // Providers installed before they offered downloads get
                // them now.
                shared.ensure_tools(driver.clone(), Arc::clone(&found));
                let tasks = found.tasks().await?;
                let downloads = found.downloads().await?;
                let download = violet_core::Download::this_platform()
                    .and_then(|platform| downloads.into_iter().find(|d| d.platform == platform));
                Ok(Offer { tasks, download })
            }
            .await;
            shared.emit(Event::Tasks { driver, result });
        });
    }

    /// Looks for a task's programs, in `folder` first.
    pub fn find_programs(&self, driver: DriverId, task: Task, folder: Option<PathBuf>) {
        let shared = Arc::clone(&self.shared);
        self.runtime.spawn(async move {
            let downloaded = shared.providers.tools(&driver);
            let programs = tasks::locate(&task, folder.as_deref(), downloaded.as_deref()).await;
            shared.emit(Event::ProgramsFound {
                driver,
                kind: task.kind,
                programs,
            });
        });
    }

    /// Builds the command a task would run, without running it.
    pub fn preview_task(
        &self,
        profile: ConnectionProfile,
        request: TaskRequest,
        folder: Option<PathBuf>,
    ) -> RequestId {
        let id = self.request();
        let shared = Arc::clone(&self.shared);
        self.runtime.spawn(async move {
            let result = shared
                .task_command(&profile, &request, folder.as_deref())
                .await;
            shared.emit(Event::TaskPreview {
                request: id,
                result,
            });
        });
        id
    }

    /// Reads the start of a file (or a folder's names) for the driver to
    /// suggest restore settings.
    pub fn inspect_input(&self, driver: DriverId, path: PathBuf) {
        let shared = Arc::clone(&self.shared);
        self.runtime.spawn(async move {
            let result = async {
                let peek_path = path.clone();
                let (head, entries, directory) =
                    tokio::task::spawn_blocking(move || tasks::peek(&peek_path))
                        .await
                        .map_err(|error| DbError::new(ErrorKind::Other, error.to_string()))?
                        .map_err(|error| DbError::new(ErrorKind::Other, error.to_string()))?;
                shared
                    .driver(&driver)
                    .await?
                    .inspect_input(&head, &entries, directory)
                    .await
            }
            .await;
            shared.emit(Event::InputInspected { path, result });
        });
    }

    /// Runs a backup or restore. The password is the one typed, else the
    /// keychain's; it reaches the program only through the variable the
    /// command names.
    pub fn run_task(
        &self,
        profile: ConnectionProfile,
        request: TaskRequest,
        folder: Option<PathBuf>,
        password: Option<Secret>,
    ) -> TaskId {
        let task = TaskId(self.next_request.fetch_add(1, Ordering::Relaxed));
        let (stop, cancel) = watch::channel(false);
        self.shared.tasks().insert(task, stop);
        let shared = Arc::clone(&self.shared);
        self.runtime.spawn(async move {
            let result = async {
                let (program, command) = shared
                    .task_command(&profile, &request, folder.as_deref())
                    .await?;
                let credentials = shared.credentials(&profile, password).await?;
                let path = PathBuf::from(&request.path);
                let batch = Arc::new(Mutex::new(Vec::new()));
                let pending = Arc::clone(&batch);
                let flush = {
                    let shared = Arc::clone(&shared);
                    move |lines: Vec<String>| {
                        if !lines.is_empty() {
                            shared.emit(Event::TaskOutput { task, lines });
                        }
                    }
                };
                // Lines go out in batches, so a chatty program does not
                // wake the interface for each one.
                let ticker = {
                    let batch = Arc::clone(&batch);
                    let flush = flush.clone();
                    tokio::spawn(async move {
                        loop {
                            tokio::time::sleep(Duration::from_millis(100)).await;
                            let lines = std::mem::take(
                                &mut *batch.lock().unwrap_or_else(PoisonError::into_inner),
                            );
                            flush(lines);
                        }
                    })
                };
                let outcome = tasks::run(
                    tasks::Run {
                        program,
                        command,
                        password: credentials.password,
                        path,
                    },
                    cancel,
                    move |line| {
                        pending
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .push(line);
                    },
                )
                .await;
                ticker.abort();
                flush(std::mem::take(
                    &mut *batch.lock().unwrap_or_else(PoisonError::into_inner),
                ));
                outcome
            }
            .await;
            shared.tasks().remove(&task);
            shared.emit(Event::TaskFinished { task, result });
        });
        task
    }

    /// Stops a running backup or restore.
    pub fn cancel_task(&self, task: TaskId) {
        if let Some(stop) = self.shared.tasks().get(&task) {
            let _ = stop.send(true);
        }
    }

    /// Opens a session for `profile`, replacing one already open.
    pub fn connect(&self, profile: ConnectionProfile, password: Option<Secret>) {
        self.connect_to(profile.id, profile, password);
    }

    /// Opens a session for `profile` as `id`, replacing one already open:
    /// another database of a saved connection, whose password is the
    /// connection's own.
    pub fn connect_to(
        &self,
        id: ConnectionId,
        profile: ConnectionProfile,
        password: Option<Secret>,
    ) {
        let shared = Arc::clone(&self.shared);
        self.runtime.spawn(async move {
            match shared.open(&profile, password).await {
                Ok((session, server)) => {
                    // A new session may see other data (another database,
                    // another user): nothing cached carries over.
                    shared.cache.invalidate(id);
                    let session: Arc<dyn Session> = Arc::from(session);
                    let previous = shared.sessions().insert(id, Arc::clone(&session));
                    if let Some(previous) = previous {
                        previous.close().await;
                    }
                    shared.reset_transaction(id, Some(&session)).await;
                    shared.emit(Event::Connected { id, server });
                }
                Err(error) => shared.emit(Event::ConnectFailed { id, error }),
            }
        });
    }

    /// Opens a throwaway session to check a profile, then closes it.
    pub fn test(&self, profile: ConnectionProfile, password: Option<Secret>) -> RequestId {
        let request = self.request();
        let shared = Arc::clone(&self.shared);
        self.runtime.spawn(async move {
            let result = match shared.open(&profile, password).await {
                Ok((session, server)) => {
                    session.close().await;
                    Ok(server)
                }
                Err(error) => Err(error),
            };
            shared.emit(Event::TestFinished { request, result });
        });
        request
    }

    pub fn disconnect(&self, id: ConnectionId) {
        self.shared.cache.invalidate(id);
        let session = self.shared.sessions().remove(&id);
        let shared = Arc::clone(&self.shared);
        self.runtime.spawn(async move {
            if let Some(session) = session {
                // Closing rolls back what an open transaction wrote.
                session.close().await;
            }
            shared.reset_transaction(id, None).await;
            shared.emit(Event::Disconnected { id });
        });
    }

    pub fn load_databases(&self, id: ConnectionId) {
        let shared = Arc::clone(&self.shared);
        self.runtime.spawn(async move {
            let result = match shared.session(id) {
                Ok(session) => session.databases().await,
                Err(error) => Err(error),
            };
            shared.emit(Event::Databases { id, result });
        });
    }

    pub fn load_schemas(&self, id: ConnectionId) {
        let shared = Arc::clone(&self.shared);
        self.runtime.spawn(async move {
            let result = match shared.session(id) {
                Ok(session) => session.schemas().await,
                Err(error) => Err(error),
            };
            shared.emit(Event::Schemas { id, result });
        });
    }

    pub fn load_objects(&self, id: ConnectionId, schema: String) {
        let shared = Arc::clone(&self.shared);
        self.runtime.spawn(async move {
            let result = match shared.session(id) {
                Ok(session) => session.objects(&schema).await,
                Err(error) => Err(error),
            };
            shared.emit(Event::Objects { id, schema, result });
        });
    }

    pub fn load_columns(&self, id: ConnectionId, schema: String, relation: String) {
        let shared = Arc::clone(&self.shared);
        self.runtime.spawn(async move {
            let result = match shared.session(id) {
                Ok(session) => session.columns(&schema, &relation).await,
                Err(error) => Err(error),
            };
            shared.emit(Event::Columns {
                id,
                schema,
                relation,
                result,
            });
        });
    }

    /// A page of `schema`'s keys matching the glob `pattern`, after the
    /// ones `cursor` follows.
    pub fn load_entries(
        &self,
        id: ConnectionId,
        schema: String,
        pattern: String,
        cursor: Option<String>,
    ) {
        /// Keys listed at a time.
        const PAGE: u32 = 500;
        let shared = Arc::clone(&self.shared);
        self.runtime.spawn(async move {
            let result = match shared.session(id) {
                Ok(session) => {
                    session
                        .entries(&schema, Some(&pattern), cursor.as_deref(), PAGE)
                        .await
                }
                Err(error) => Err(error),
            };
            shared.emit(Event::Entries {
                id,
                schema,
                pattern,
                more: cursor.is_some(),
                result,
            });
        });
    }

    /// Looks up at most `limit` of `schema`'s keys matching `glob`, for
    /// completing keys starting with `prefix`.
    pub fn match_keys(
        &self,
        id: ConnectionId,
        schema: String,
        prefix: String,
        glob: String,
        limit: u32,
    ) {
        let shared = Arc::clone(&self.shared);
        self.runtime.spawn(async move {
            let result = match shared.session(id) {
                Ok(session) => session.entries(&schema, Some(&glob), None, limit).await,
                Err(error) => Err(error),
            };
            shared.emit(Event::KeysMatching {
                id,
                schema,
                prefix,
                result,
            });
        });
    }

    /// Reads the foreign keys from or to `schema`'s tables, for completing
    /// joins. Databases the dialect has no query for answer with none.
    pub fn load_foreign_keys(&self, id: ConnectionId, schema: String) {
        let shared = Arc::clone(&self.shared);
        let limits = self.limits;
        self.runtime.spawn(async move {
            let result = async {
                let session = shared.session(id)?;
                if session.language() != Language::Sql {
                    return Ok(Vec::new());
                }
                let Some(sql) = session.dialect().foreign_keys(&schema) else {
                    return Ok(Vec::new());
                };
                Ok(foreign_keys_from(&session.execute(&sql, limits).await?))
            }
            .await;
            shared.emit(Event::ForeignKeys { id, schema, result });
        });
    }

    /// Runs `sql` on the open session for `id`.
    pub fn execute(&self, id: ConnectionId, sql: String, options: ExecOptions) -> RequestId {
        self.execute_script(id, None, sql, options, TxControl::default())
    }

    /// Runs `sql` on the open session for `id`; for a driver that is not
    /// SQL, in `schema` when given (a data view's, the tree's), leaving
    /// the editor's state alone.
    pub fn execute_in(
        &self,
        id: ConnectionId,
        schema: Option<String>,
        sql: String,
        options: ExecOptions,
    ) -> RequestId {
        self.execute_script(id, schema, sql, options, TxControl::default())
    }

    /// Runs SQL the user wrote, whose own transaction statements `control`
    /// describes, on the open session for `id`; `schema` as for
    /// [`Backend::execute_in`].
    pub fn execute_script(
        &self,
        id: ConnectionId,
        schema: Option<String>,
        sql: String,
        options: ExecOptions,
        control: TxControl,
    ) -> RequestId {
        let request = self.request();
        let shared = Arc::clone(&self.shared);
        let limits = self.limits;
        self.runtime.spawn(async move {
            let result = shared
                .query(request, id, sql, options, limits, schema, control)
                .await;
            shared.emit(Event::QueryFinished { request, result });
        });
        request
    }

    /// Commits or rolls back `id`'s transaction, or changes its mode; a
    /// `TransactionDone` answers. The mode outlives the session: the next
    /// one starts in it.
    pub fn transaction(&self, id: ConnectionId, command: TxCommand) {
        let shared = Arc::clone(&self.shared);
        let limits = self.limits;
        self.runtime.spawn(async move {
            let result = shared.transact(id, command, limits).await;
            shared.emit(Event::TransactionDone {
                id,
                command,
                result,
            });
        });
    }

    /// Counts the rows a single read-only statement returns; for a driver
    /// that is not SQL, run in `schema`.
    pub fn count(&self, id: ConnectionId, schema: Option<String>, sql: String) -> RequestId {
        let request = self.request();
        let shared = Arc::clone(&self.shared);
        let limits = self.limits;
        self.runtime.spawn(async move {
            let result = async {
                let session = shared.session(id)?;
                if session.language() != Language::Sql {
                    let schema = schema.unwrap_or_default();
                    return session.count(&schema, &sql).await?.ok_or_else(|| {
                        DbError::unsupported("This command's rows cannot be counted")
                    });
                }
                let count = session.dialect().count(&sql);
                let options = ExecOptions {
                    cache: CachePolicy::Use,
                    page: None,
                };
                let output = shared
                    .query(
                        request,
                        id,
                        count,
                        options,
                        limits,
                        None,
                        TxControl::default(),
                    )
                    .await?;
                output
                    .sets
                    .first()
                    .and_then(|set| set.rows.get(0, 0).flatten())
                    .and_then(|count| count.parse().ok())
                    .ok_or_else(|| DbError::new(ErrorKind::Other, "The count returned no number"))
            }
            .await;
            shared.emit(Event::Counted { request, result });
        });
        request
    }

    /// Fetches the provider catalogue.
    pub fn load_providers(&self) {
        let shared = Arc::clone(&self.shared);
        self.runtime.spawn(async move {
            let result = shared.providers.available().await;
            shared.emit(Event::Providers(result));
        });
    }

    /// Installs or updates a provider. Sessions already open keep the
    /// version they started with.
    pub fn install_provider(&self, id: DriverId) {
        let shared = Arc::clone(&self.shared);
        self.runtime.spawn(async move {
            let result = shared.providers.install(&id).await;
            if let Ok(driver) = &result {
                shared.registry.add(Arc::clone(driver));
                shared.ensure_tools(id.clone(), Arc::clone(driver));
            }
            let result = result.map(|driver| driver.info());
            shared.emit(Event::ProviderInstalled { id, result });
        });
    }

    pub fn remove_provider(&self, id: DriverId) {
        let shared = Arc::clone(&self.shared);
        self.runtime.spawn(async move {
            let result = shared.providers.remove(&id).await;
            if result.is_ok() {
                shared.registry.remove(&id);
            }
            shared.emit(Event::ProviderRemoved { id, result });
        });
    }

    /// Asks the server to stop a running query. Its `QueryFinished` then
    /// carries a cancelled error.
    pub fn cancel(&self, request: RequestId) {
        let Some(canceller) = self.shared.running().get(&request).cloned() else {
            return;
        };
        self.runtime.spawn(async move {
            if let Err(error) = canceller.cancel().await {
                log::warn!("cancel failed: {error}");
            }
        });
    }
}

/// Groups the rows of [`violet_core::Dialect::foreign_keys`] into keys.
/// Rows with a missing cell are left out.
fn foreign_keys_from(sets: &[ResultSet]) -> Vec<ForeignKey> {
    let mut keys: Vec<ForeignKey> = Vec::new();
    let Some(set) = sets.iter().find(|set| set.rows.width() >= 8) else {
        return keys;
    };
    for row in set.rows.iter() {
        let cells: Option<Vec<&str>> = (0..8).map(|i| row.get(i).flatten()).collect();
        let Some(cells) = cells else { continue };
        let [
            _,
            name,
            schema,
            table,
            column,
            ref_schema,
            ref_table,
            ref_column,
        ] = cells[..]
        else {
            continue;
        };
        let same = keys
            .last_mut()
            .filter(|key| key.name == name && key.schema == schema && key.table == table);
        match same {
            Some(key) => {
                key.columns.push(column.to_owned());
                key.ref_columns.push(ref_column.to_owned());
            }
            None => keys.push(ForeignKey {
                name: name.to_owned(),
                schema: schema.to_owned(),
                table: table.to_owned(),
                columns: vec![column.to_owned()],
                ref_schema: ref_schema.to_owned(),
                ref_table: ref_table.to_owned(),
                ref_columns: vec![ref_column.to_owned()],
            }),
        }
    }
    keys
}

impl Drop for Backend {
    fn drop(&mut self) {
        let sessions: Vec<_> = self.shared.sessions().drain().map(|(_, s)| s).collect();
        self.runtime.block_on(async {
            for session in sessions {
                // Do not let one stuck server hold the window open.
                let _ = tokio::time::timeout(Duration::from_secs(2), session.close()).await;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn foreign_key_rows_group_into_keys() {
        let mut set = ResultSet::default();
        for row in [
            [
                "s",
                "fk_a",
                "s",
                "orders",
                "customer_id",
                "s",
                "customers",
                "id",
            ],
            ["s", "fk_b", "s", "lines", "order_id", "s", "orders", "id"],
            ["s", "fk_b", "s", "lines", "order_rev", "s", "orders", "rev"],
        ] {
            set.rows.push(row.map(Some));
        }
        let keys = foreign_keys_from(&[set]);
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0].table, "orders");
        assert_eq!(keys[0].ref_table, "customers");
        assert_eq!(keys[1].columns, ["order_id", "order_rev"]);
        assert_eq!(keys[1].ref_columns, ["id", "rev"]);
    }
}
