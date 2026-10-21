//! A session living in its own provider instance.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use tokio::sync::Mutex;
use violet_core::{
    Canceller, ColumnInfo, DbError, DbObject, Dialect, EntryPage, ErrorKind, ExecLimits, Language,
    Result, ResultSet,
    ServerInfo, Session, SqlDialect,
};
use wasmtime::component::ResourceAny;
use wasmtime::{Store, UpdateDeadline};

use crate::Provider;
use crate::bindings::exports::violet::provider::{
    data as data_api, provider as provider_api, tools as tools_api,
};
use crate::bindings::types as wit;
use crate::convert;
use crate::host::HostState;
use crate::socket::{Kill, Target};

/// Instructions one call into a provider may run: tens of seconds of
/// busy work, far more than reading the rows Violet keeps (and skipping
/// the rest) needs. Waiting on the server costs nothing; a loop runs out.
const FUEL_PER_CALL: u64 = 50_000_000_000;

/// A fresh instance of a provider, alone in its store.
pub(crate) struct Instance {
    pub(crate) store: Store<HostState>,
    world: Exports,
    kill: Arc<Kill>,
}

/// What an instance exports.
pub(crate) struct Exports {
    provider: provider_api::Guest,
    tools: Option<tools_api::Guest>,
    data: Option<data_api::Guest>,
}

impl Exports {
    fn data(&self) -> Option<&data_api::Guest> {
        self.data.as_ref()
    }

    fn violet_provider_provider(&self) -> &provider_api::Guest {
        &self.provider
    }

    pub(crate) fn tools(&self) -> Option<&tools_api::Guest> {
        self.tools.as_ref()
    }
}

impl Instance {
    /// Instantiates `provider`, allowed to reach `target` only.
    #[hotpath::measure]
    pub(crate) async fn start(provider: &Provider, target: Option<Target>) -> Result<Self> {
        let kill = Arc::new(Kill::default());
        let mut store = Store::new(&provider.engine, HostState::new(target, Arc::clone(&kill)));
        store.limiter(|state| &mut state.limits);
        // Yield at every tick; once the session is killed, trap instead.
        store.epoch_deadline_callback(|context| {
            if context.data().kill.fired() {
                Err(wasmtime::Error::msg("the session was closed"))
            } else {
                Ok(UpdateDeadline::Yield(1))
            }
        });
        store.set_epoch_deadline(1);
        refuel(&mut store);
        let instance = provider
            .pre
            .instantiate_async(&mut store)
            .await
            .map_err(|error| crashed(&error))?;
        let world = Exports {
            provider: provider
                .api
                .load(&mut store, &instance)
                .map_err(|error| crashed(&error))?,
            tools: match &provider.tools {
                Some(tools) => Some(
                    tools
                        .load(&mut store, &instance)
                        .map_err(|error| crashed(&error))?,
                ),
                None => None,
            },
            data: match &provider.data {
                Some(data) => Some(
                    data.load(&mut store, &instance)
                        .map_err(|error| crashed(&error))?,
                ),
                None => None,
            },
        };
        Ok(Self { store, world, kill })
    }

    pub(crate) async fn info(&mut self) -> Result<wit::ProviderInfo> {
        let info = self
            .world
            .violet_provider_provider()
            .call_info(&mut self.store)
            .await
            .map_err(|error| crashed(&error))?;
        self.store.data_mut().set_provider(&info.id);
        Ok(info)
    }
}

impl Instance {
    /// The provider's language and login, when it says.
    pub(crate) async fn extras(&mut self) -> Result<Option<data_api::ProviderExtras>> {
        let Some(data) = self.world.data() else {
            return Ok(None);
        };
        refuel(&mut self.store);
        data.call_extras(&mut self.store)
            .await
            .map(Some)
            .map_err(|error| crashed(&error))
    }

    pub(crate) fn exports(&self) -> &Exports {
        &self.world
    }
}

/// Gives the next call its budget of instructions.
pub(crate) fn refuel(store: &mut Store<HostState>) {
    // Only fails when fuel is not enabled, which the engine always has.
    let _ = store.set_fuel(FUEL_PER_CALL);
}

/// A trap or a broken invariant: the instance cannot be trusted further.
pub(crate) fn crashed(error: &wasmtime::Error) -> DbError {
    if error.downcast_ref::<wasmtime::Trap>() == Some(&wasmtime::Trap::OutOfFuel) {
        return DbError::new(
            ErrorKind::Provider,
            "The provider ran too long without waiting on the server and was stopped",
        );
    }
    DbError::new(
        ErrorKind::Provider,
        format!("The provider stopped: {error:#}"),
    )
}

/// What a session holds while it is alive.
struct Live {
    instance: Instance,
    session: ResourceAny,
}

/// A session opened by a provider.
pub(crate) struct WasmSession {
    live: Mutex<Option<Live>>,
    kill: Arc<Kill>,
    /// Set after a trap: the instance is gone, the session with it.
    broken: AtomicBool,
    dialect: SqlDialect,
    language: Language,
    cancel: Option<Arc<CancelPlan>>,
}

/// How to cancel: a fresh instance of the same provider, the session's
/// target and parameters (without the password) and its ticket.
struct CancelPlan {
    provider: Arc<Provider>,
    target: Target,
    params: wit::ConnectParams,
    ticket: Vec<u8>,
}

impl WasmSession {
    #[hotpath::measure]
    pub(crate) async fn open(
        provider: Arc<Provider>,
        target: Target,
        params: wit::ConnectParams,
        dialect: SqlDialect,
        language: Language,
        cancellable: bool,
    ) -> Result<Self> {
        let mut instance = Instance::start(&provider, Some(target.clone())).await?;
        let id = instance.info().await?.id;
        instance.store.data_mut().set_provider(&id);
        let exports = instance.world.violet_provider_provider();
        let session = exports
            .call_connect(&mut instance.store, &params)
            .await
            .map_err(|error| crashed(&error))?
            .map_err(convert::error)?;
        let ticket = if cancellable {
            exports
                .session()
                .call_cancel_ticket(&mut instance.store, session)
                .await
                .map_err(|error| crashed(&error))?
        } else {
            None
        };
        let kill = Arc::clone(&instance.kill);
        let cancel = ticket.map(|ticket| {
            Arc::new(CancelPlan {
                provider,
                target,
                params: wit::ConnectParams {
                    password: None,
                    ..params
                },
                ticket,
            })
        });
        Ok(Self {
            live: Mutex::new(Some(Live { instance, session })),
            kill,
            broken: AtomicBool::new(false),
            dialect,
            language,
            cancel,
        })
    }

    /// Runs `call` on the live instance, one at a time. A trap ends the
    /// session.
    async fn with<T>(
        &self,
        label: &'static str,
        call: impl AsyncFnOnce(
            &mut Store<HostState>,
            &Exports,
            ResourceAny,
        ) -> wasmtime::Result<std::result::Result<T, wit::DbError>>,
    ) -> Result<T> {
        if self.broken.load(Ordering::SeqCst) {
            return Err(gone());
        }
        let mut live = self.live.lock().await;
        let Some(Live { instance, session }) = live.as_mut() else {
            return Err(gone());
        };
        refuel(&mut instance.store);
        let outcome = hotpath::measure_block!(
            label,
            call(&mut instance.store, &instance.world, *session).await
        );
        match outcome {
            Ok(result) => result.map_err(convert::error),
            Err(error) => {
                self.broken.store(true, Ordering::SeqCst);
                *live = None;
                if self.kill.fired() {
                    Err(DbError::new(ErrorKind::Cancelled, "The session was closed"))
                } else {
                    log::error!(target: "violet::provider", "instance trapped in {label}: {error:#}");
                    Err(crashed(&error))
                }
            }
        }
    }
}

fn unsupported() -> wit::DbError {
    wit::DbError {
        kind: wit::ErrorKind::Unsupported,
        message: "The provider does not support this".into(),
        code: None,
        detail: None,
        hint: None,
        position: None,
    }
}

fn gone() -> DbError {
    DbError::new(
        ErrorKind::Connection,
        "The provider stopped; reconnect to continue",
    )
}

#[async_trait]
impl Session for WasmSession {
    async fn server_info(&self) -> Result<ServerInfo> {
        self.with("WasmSession::server_info", async |store, world, session| {
            world
                .violet_provider_provider()
                .session()
                .call_server_info(store, session)
                .await
        })
        .await
        .map(convert::server_info)
    }

    async fn databases(&self) -> Result<Vec<String>> {
        self.with("WasmSession::databases", async |store, world, session| {
            world
                .violet_provider_provider()
                .session()
                .call_databases(store, session)
                .await
        })
        .await
    }

    async fn schemas(&self) -> Result<Vec<String>> {
        self.with("WasmSession::schemas", async |store, world, session| {
            world
                .violet_provider_provider()
                .session()
                .call_schemas(store, session)
                .await
        })
        .await
    }

    async fn objects(&self, schema: &str) -> Result<Vec<DbObject>> {
        let objects = self
            .with("WasmSession::objects", async |store, world, session| {
                world
                    .violet_provider_provider()
                    .session()
                    .call_objects(store, session, schema)
                    .await
            })
            .await?;
        Ok(objects
            .into_iter()
            .map(|object| convert::object(schema, object))
            .collect())
    }

    async fn columns(&self, schema: &str, relation: &str) -> Result<Vec<ColumnInfo>> {
        let columns = self
            .with("WasmSession::columns", async |store, world, session| {
                world
                    .violet_provider_provider()
                    .session()
                    .call_columns(store, session, schema, relation)
                    .await
            })
            .await?;
        Ok(columns.into_iter().map(convert::column).collect())
    }

    async fn execute(&self, sql: &str, limits: ExecLimits) -> Result<Vec<ResultSet>> {
        let max_rows = u32::try_from(limits.max_rows).unwrap_or(u32::MAX);
        let sets = self
            .with("WasmSession::execute", async |store, world, session| {
                world
                    .violet_provider_provider()
                    .session()
                    .call_execute(store, session, sql, max_rows)
                    .await
            })
            .await?;
        let mut converted = Vec::with_capacity(sets.len());
        for set in sets {
            let mut set = convert::result_set(set)?;
            // Never hold more than asked for, whatever the provider sent.
            if set.rows.len() > limits.max_rows {
                set.rows.truncate(limits.max_rows);
                set.truncated = true;
            }
            converted.push(set);
        }
        Ok(converted)
    }

    async fn change_token(&self) -> Result<Option<String>> {
        self.with(
            "WasmSession::change_token",
            async |store, world, session| {
                world
                    .violet_provider_provider()
                    .session()
                    .call_change_token(store, session)
                    .await
            },
        )
        .await
    }

    fn canceller(&self) -> Option<Box<dyn Canceller>> {
        self.cancel
            .as_ref()
            .map(|plan| Box::new(WasmCanceller(Arc::clone(plan))) as Box<dyn Canceller>)
    }

    fn dialect(&self) -> &dyn Dialect {
        &self.dialect
    }

    fn language(&self) -> Language {
        self.language
    }

    async fn entries(
        &self,
        schema: &str,
        pattern: Option<&str>,
        cursor: Option<&str>,
        limit: u32,
    ) -> Result<EntryPage> {
        let page = self
            .with("WasmSession::entries", async |store, world, session| {
                let Some(data) = world.data() else {
                    return Ok(Err(unsupported()));
                };
                data.call_entries(store, session, schema, pattern, cursor, limit)
                    .await
            })
            .await?;
        Ok(convert::entry_page(schema, page, limit))
    }

    async fn page(
        &self,
        schema: &str,
        statement: &str,
        offset: u64,
        limit: u32,
    ) -> Result<Vec<ResultSet>> {
        let sets = self
            .with("WasmSession::page", async |store, world, session| {
                let Some(data) = world.data() else {
                    return Ok(Err(unsupported()));
                };
                data.call_page(store, session, schema, statement, offset, limit)
                    .await
            })
            .await?;
        let mut converted = Vec::with_capacity(sets.len());
        for set in sets {
            let mut set = convert::result_set(set)?;
            // One row past the page tells another follows; no more.
            set.rows.truncate(limit as usize + 1);
            converted.push(set);
        }
        Ok(converted)
    }

    async fn run_in(&self, schema: &str, statements: &str, limits: ExecLimits) -> Result<Vec<ResultSet>> {
        let max_rows = u32::try_from(limits.max_rows).unwrap_or(u32::MAX);
        let sets = self
            .with("WasmSession::run_in", async |store, world, session| {
                let Some(data) = world.data() else {
                    return Ok(Err(unsupported()));
                };
                data.call_run(store, session, schema, statements, max_rows)
                    .await
            })
            .await?;
        let mut converted = Vec::with_capacity(sets.len());
        for set in sets {
            let mut set = convert::result_set(set)?;
            if set.rows.len() > limits.max_rows {
                set.rows.truncate(limits.max_rows);
                set.truncated = true;
            }
            converted.push(set);
        }
        Ok(converted)
    }

    async fn count(&self, schema: &str, statement: &str) -> Result<Option<u64>> {
        self.with("WasmSession::count", async |store, world, session| {
            let Some(data) = world.data() else {
                return Ok(Err(unsupported()));
            };
            data.call_count(store, session, schema, statement).await
        })
        .await
    }

    async fn close(&self) {
        // An idle provider may say goodbye to its server; a busy one is
        // stopped first (its socket waits end and its code traps).
        let mut live = match self.live.try_lock() {
            Ok(live) => live,
            Err(_) => {
                self.kill.fire();
                self.live.lock().await
            }
        };
        if let Some(Live {
            mut instance,
            session,
        }) = live.take()
            && !self.broken.load(Ordering::SeqCst)
        {
            refuel(&mut instance.store);
            let goodbye = session.resource_drop_async(&mut instance.store);
            let _ = tokio::time::timeout(std::time::Duration::from_secs(2), goodbye).await;
        }
        self.kill.fire();
    }
}

impl Drop for WasmSession {
    fn drop(&mut self) {
        self.kill.fire();
    }
}

struct WasmCanceller(Arc<CancelPlan>);

#[async_trait]
impl Canceller for WasmCanceller {
    async fn cancel(&self) -> Result<()> {
        let plan = &self.0;
        let mut instance = Instance::start(&plan.provider, Some(plan.target.clone())).await?;
        instance
            .world
            .violet_provider_provider()
            .call_cancel(&mut instance.store, &plan.params, &plan.ticket)
            .await
            .map_err(|error| crashed(&error))?
            .map_err(convert::error)
    }
}
