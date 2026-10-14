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
    DriverId, DriverInfo, ErrorKind, ExecLimits, ForeignKey, OrderBy, Page, PageInfo, QueryOutput,
    Result, ResultSet, Secret, ServerInfo, Session, Task, TaskKind, TaskRequest, TaskSettings,
    ToolCommand,
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

/// Identifies one query or connection test, to match its result and to
/// cancel it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RequestId(u64);

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
        result: Result<Vec<Task>>,
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
    /// Lines a running task's program printed.
    TaskOutput {
        task: TaskId,
        lines: Vec<String>,
    },
    TaskFinished {
        task: TaskId,
        result: Result<TaskSummary>,
    },
}

type Sessions = HashMap<ConnectionId, Arc<dyn Session>>;
type Running = HashMap<RequestId, Arc<dyn Canceller>>;
type RunningTasks = HashMap<TaskId, watch::Sender<bool>>;

/// Shared between the backend handle and the tasks it spawns.
struct Shared {
    registry: DriverRegistry,
    providers: Arc<dyn ProviderStore>,
    secrets: Arc<dyn SecretStore>,
    cache: Arc<dyn ResultCache>,
    sessions: Mutex<Sessions>,
    running: Mutex<Running>,
    tasks: Mutex<RunningTasks>,
    events: Mutex<Sender<Event>>,
    notify: Box<dyn Notify>,
}

impl Shared {
    fn sessions(&self) -> MutexGuard<'_, Sessions> {
        self.sessions.lock().unwrap_or_else(PoisonError::into_inner)
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
        &self,
        profile: &ConnectionProfile,
        request: &TaskRequest,
        folder: Option<&Path>,
    ) -> Result<(PathBuf, ToolCommand)> {
        let driver = self.driver(&profile.driver).await?;
        let task = self.task(&driver, request.kind).await?;
        let mut request = request.clone();
        request.settings = task.normalize(&request.settings);
        let command = driver.task_command(profile, &request).await?;
        command.check(&task)?;
        let folders = tasks::search_folders(&task, folder);
        let program = tasks::find(&command.program, &folders).ok_or_else(|| {
            DbError::new(
                ErrorKind::Other,
                format!(
                    "{} was not found. Install the database's client tools, or pick the \
                     folder they are in.",
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

    /// The driver for `id`, installing its provider first if needed.
    #[hotpath::measure]
    async fn driver(&self, id: &DriverId) -> Result<Arc<dyn Driver>> {
        if let Some(driver) = self.registry.get(id) {
            return Ok(driver);
        }
        self.emit(Event::ProviderInstalling { id: id.clone() });
        let result = self.providers.install(id).await;
        if let Ok(driver) = &result {
            self.registry.add(Arc::clone(driver));
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

    /// Runs `sql` on `id`'s session as `options` say, through the cache.
    #[hotpath::measure]
    async fn query(
        &self,
        request: RequestId,
        id: ConnectionId,
        sql: String,
        options: ExecOptions,
        limits: ExecLimits,
    ) -> Result<QueryOutput> {
        let session = self.session(id)?;
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
        let cacheable = options.cache != CachePolicy::Off;
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
        if !cacheable {
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

    /// How a data view reads a relation, in the session's dialect.
    #[must_use]
    pub fn browse_sql(
        &self,
        id: ConnectionId,
        schema: &str,
        relation: &str,
        filter: Option<&str>,
        order: Option<&OrderBy>,
    ) -> Option<String> {
        let session = self.shared.session(id).ok()?;
        Some(session.dialect().browse(schema, relation, filter, order))
    }

    /// How the session's dialect explains `sql`.
    #[must_use]
    pub fn explain_sql(&self, id: ConnectionId, sql: &str, analyze: bool) -> Option<String> {
        let session = self.shared.session(id).ok()?;
        Some(session.dialect().explain(sql, analyze))
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
            let result = async { shared.driver(&driver).await?.tasks().await }.await;
            shared.emit(Event::Tasks { driver, result });
        });
    }

    /// Looks for a task's programs, in `folder` first.
    pub fn find_programs(&self, driver: DriverId, task: Task, folder: Option<PathBuf>) {
        let shared = Arc::clone(&self.shared);
        self.runtime.spawn(async move {
            let programs = tasks::locate(&task, folder.as_deref()).await;
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
        let shared = Arc::clone(&self.shared);
        self.runtime.spawn(async move {
            let id = profile.id;
            match shared.open(&profile, password).await {
                Ok((session, server)) => {
                    // A new session may see other data (another database,
                    // another user): nothing cached carries over.
                    shared.cache.invalidate(id);
                    let previous = shared.sessions().insert(id, Arc::from(session));
                    if let Some(previous) = previous {
                        previous.close().await;
                    }
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
                session.close().await;
            }
            shared.emit(Event::Disconnected { id });
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

    /// Reads the foreign keys from or to `schema`'s tables, for completing
    /// joins. Databases the dialect has no query for answer with none.
    pub fn load_foreign_keys(&self, id: ConnectionId, schema: String) {
        let shared = Arc::clone(&self.shared);
        let limits = self.limits;
        self.runtime.spawn(async move {
            let result = async {
                let session = shared.session(id)?;
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
        let request = self.request();
        let shared = Arc::clone(&self.shared);
        let limits = self.limits;
        self.runtime.spawn(async move {
            let result = shared.query(request, id, sql, options, limits).await;
            shared.emit(Event::QueryFinished { request, result });
        });
        request
    }

    /// Counts the rows a single read-only statement returns.
    pub fn count(&self, id: ConnectionId, sql: String) -> RequestId {
        let request = self.request();
        let shared = Arc::clone(&self.shared);
        let limits = self.limits;
        self.runtime.spawn(async move {
            let result = async {
                let count = shared.session(id)?.dialect().count(&sql);
                let options = ExecOptions {
                    cache: CachePolicy::Use,
                    page: None,
                };
                let output = shared.query(request, id, count, options, limits).await?;
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
