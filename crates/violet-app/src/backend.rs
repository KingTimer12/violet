//! Running database work off the interface thread.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use tokio::runtime::Runtime;
use violet_core::ports::SecretStore;
use violet_core::{
    Canceller, ColumnInfo, ConnectionId, ConnectionProfile, Credentials, DbError, DbObject,
    ErrorKind, ExecLimits, OrderBy, Page, PageInfo, QueryOutput, Result, Secret, ServerInfo,
    Session,
};

use crate::registry::DriverRegistry;

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

/// How to run a query.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExecOptions {
    /// Read only this window of the rows. The query must be a single
    /// read-only statement.
    pub page: Option<Page>,
}

impl ExecOptions {
    /// Every row at once.
    pub const WHOLE: Self = Self { page: None };
}

/// Identifies one query or connection test, to match its result and to
/// cancel it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RequestId(u64);

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
}

type Sessions = HashMap<ConnectionId, Arc<dyn Session>>;
type Running = HashMap<RequestId, Arc<dyn Canceller>>;

/// Shared between the backend handle and the tasks it spawns.
struct Shared {
    registry: DriverRegistry,
    secrets: Arc<dyn SecretStore>,
    sessions: Mutex<Sessions>,
    running: Mutex<Running>,
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

    async fn open(
        self: &Arc<Self>,
        profile: &ConnectionProfile,
        typed: Option<Secret>,
    ) -> Result<(Box<dyn Session>, ServerInfo)> {
        let driver = self
            .registry
            .get(&profile.driver)
            .ok_or_else(|| DbError::unsupported(format!("No driver for \"{}\"", profile.driver)))?;
        let credentials = self.credentials(profile, typed).await?;
        let session = driver.connect(profile, &credentials).await?;
        let server = session.server_info().await?;
        Ok((session, server))
    }

    /// Runs `sql` on `id`'s session as `options` say.
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
        if let Some(canceller) = session.canceller() {
            self.running().insert(request, Arc::from(canceller));
        }
        let started = Instant::now();
        let result = session.execute(&sent, limits).await;
        self.running().remove(&request);
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
        };
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
    /// Starts the runtime. `notify` is called from worker threads whenever
    /// an event is queued.
    pub fn start(
        registry: DriverRegistry,
        secrets: Arc<dyn SecretStore>,
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
                secrets,
                sessions: Mutex::default(),
                running: Mutex::default(),
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

    /// The most rows one result may hold.
    #[must_use]
    pub fn max_rows(&self) -> usize {
        self.limits.max_rows
    }

    fn request(&self) -> RequestId {
        RequestId(self.next_request.fetch_add(1, Ordering::Relaxed))
    }

    /// Opens a session for `profile`, replacing one already open.
    pub fn connect(&self, profile: ConnectionProfile, password: Option<Secret>) {
        let shared = Arc::clone(&self.shared);
        self.runtime.spawn(async move {
            let id = profile.id;
            match shared.open(&profile, password).await {
                Ok((session, server)) => {
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
                let output = shared
                    .query(request, id, count, ExecOptions::WHOLE, limits)
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
