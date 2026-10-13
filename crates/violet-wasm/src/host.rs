//! What an instance can call: its socket and the log, plus a WASI with
//! nothing in it.

use std::sync::Arc;

use wasmtime::component::{Resource, ResourceTable};
use wasmtime::{StoreLimits, StoreLimitsBuilder};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};

use crate::bindings::types::{DbError, ErrorKind};
use crate::bindings::violet::provider::{logging, network};
use crate::socket::{Kill, MAX_SOCKETS, Socket, Target, error};

/// Memory one instance may use.
const MAX_MEMORY: usize = 512 << 20;

/// Longest log message kept.
const MAX_LOG: usize = 4096;

/// One instance's state.
pub struct HostState {
    wasi: WasiCtx,
    table: ResourceTable,
    pub(crate) limits: StoreLimits,
    /// Where the socket may go; `None` when describing the provider.
    target: Option<Target>,
    pub(crate) kill: Arc<Kill>,
    sockets: usize,
    /// For log lines.
    provider: String,
}

impl HostState {
    pub(crate) fn new(target: Option<Target>, kill: Arc<Kill>) -> Self {
        // No preopened directories, environment, arguments or stdio, and
        // WASI's own sockets refused outright: the provider's only way out
        // is `network`.
        let wasi = WasiCtx::builder()
            .allow_tcp(false)
            .allow_udp(false)
            .allow_ip_name_lookup(false)
            .socket_addr_check(|_, _| Box::pin(async { false }))
            .build();
        Self {
            wasi,
            table: ResourceTable::new(),
            limits: StoreLimitsBuilder::new()
                .memory_size(MAX_MEMORY)
                .instances(16)
                .tables(64)
                .memories(16)
                .trap_on_grow_failure(true)
                .build(),
            target,
            kill,
            sockets: 0,
            provider: "provider".into(),
        }
    }

    pub(crate) fn set_provider(&mut self, id: &str) {
        self.provider = id.chars().take(32).collect();
    }

    fn target(&self) -> Result<&Target, DbError> {
        if self.sockets >= MAX_SOCKETS {
            return Err(error(ErrorKind::Connection, "Too many open sockets"));
        }
        self.target
            .as_ref()
            .ok_or_else(|| error(ErrorKind::Unsupported, "No connection is being opened"))
    }

    fn adopt(&mut self, socket: Socket) -> Result<Resource<Socket>, DbError> {
        let resource = self
            .table
            .push(socket)
            .map_err(|e| error(ErrorKind::Other, e.to_string()))?;
        self.sockets += 1;
        Ok(resource)
    }

    fn socket(&mut self, resource: &Resource<Socket>) -> Result<&mut Socket, DbError> {
        self.table
            .get_mut(resource)
            .map_err(|e| error(ErrorKind::Other, e.to_string()))
    }
}

impl WasiView for HostState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl crate::bindings::types::Host for HostState {}

impl network::Host for HostState {
    async fn connect(&mut self) -> Result<Resource<Socket>, DbError> {
        let target = self.target()?.clone();
        let socket = Socket::connect(&target, Arc::clone(&self.kill)).await?;
        self.adopt(socket)
    }

    async fn connect_local(&mut self, name: String) -> Result<Resource<Socket>, DbError> {
        let target = self.target()?.clone();
        let socket = Socket::connect_local(&target, &name, Arc::clone(&self.kill)).await?;
        self.adopt(socket)
    }
}

impl network::HostSocket for HostState {
    async fn read(&mut self, socket: Resource<Socket>, max: u32) -> Result<Vec<u8>, DbError> {
        self.socket(&socket)?.read(max).await
    }

    async fn write(&mut self, socket: Resource<Socket>, bytes: Vec<u8>) -> Result<(), DbError> {
        self.socket(&socket)?.write(&bytes).await
    }

    async fn start_tls(&mut self, socket: Resource<Socket>) -> Result<(), DbError> {
        self.socket(&socket)?.start_tls().await
    }

    async fn is_tls(&mut self, socket: Resource<Socket>) -> bool {
        self.socket(&socket).is_ok_and(|socket| socket.is_tls())
    }

    async fn drop(&mut self, socket: Resource<Socket>) -> wasmtime::Result<()> {
        self.table.delete(socket)?;
        self.sockets = self.sockets.saturating_sub(1);
        Ok(())
    }
}

impl logging::Host for HostState {
    async fn log(&mut self, level: logging::Level, message: String) {
        let level = match level {
            logging::Level::Error => log::Level::Error,
            logging::Level::Warn => log::Level::Warn,
            logging::Level::Info => log::Level::Info,
            logging::Level::Debug => log::Level::Debug,
        };
        let mut message = message;
        if message.len() > MAX_LOG {
            let mut end = MAX_LOG;
            while !message.is_char_boundary(end) {
                end -= 1;
            }
            message.truncate(end);
            message.push('…');
        }
        // One line per message, whatever the provider sent.
        let message = message.replace(['\n', '\r'], " ");
        log::log!(target: "violet::provider", level, "[{}] {message}", self.provider);
    }
}
