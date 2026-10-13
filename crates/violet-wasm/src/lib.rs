//! Runs database providers compiled to WebAssembly, as Violet drivers.
//!
//! A provider is a component built with `violet-sdk` against
//! `violet-sdk/wit/provider.wit`. Each connection gets its own instance,
//! with its own memory, and the instance gets nothing but:
//!
//! - one kind of socket, to the host and port of the profile it serves
//!   ([`socket`]), with TLS done here, against the web PKI roots;
//! - a log;
//! - the bare WASI a Rust program needs to start (clocks, randomness), with
//!   no files, environment, arguments or sockets of its own.
//!
//! Memory is capped, runaway code yields back to the async runtime and is
//! stopped when its session closes, and every value a provider returns is
//! checked before Violet uses it.

mod bindings;
mod convert;
mod driver;
mod host;
mod session;
mod socket;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use violet_core::{DbError, DriverInfo, ErrorKind, Result};
use wasmtime::component::{Component, Linker};
use wasmtime::{Config, Engine};

pub use convert::valid_id;
pub use driver::{Loader, WasmDriver};

use crate::bindings::ProviderWorldPre;
use crate::host::HostState;

/// The provider contract version this host speaks; see `violet-sdk`.
pub const API_VERSION: &str = "0.1.0";

/// How often running guest code yields to the async runtime.
const TICK: Duration = Duration::from_millis(10);

/// The engine every provider runs on. Cheap to clone.
#[derive(Clone)]
pub struct Runtime {
    engine: Engine,
    linker: Arc<Linker<HostState>>,
}

impl Runtime {
    /// Sets the engine up. Compiled code is cached under `cache_dir`, so a
    /// provider compiles once rather than on every start.
    pub fn new(cache_dir: Option<&Path>) -> Result<Self> {
        let mut config = Config::new();
        config
            .wasm_component_model(true)
            .epoch_interruption(true)
            .max_wasm_stack(2 << 20);
        if let Some(dir) = cache_dir {
            let mut cache = wasmtime::CacheConfig::new();
            cache.with_directory(dir);
            match wasmtime::Cache::new(cache) {
                Ok(cache) => {
                    config.cache(Some(cache));
                }
                Err(error) => log::warn!("provider code cache disabled: {error}"),
            }
        }
        let engine = Engine::new(&config).map_err(failure)?;

        let mut linker = Linker::new(&engine);
        wasmtime_wasi::p2::add_to_linker_async(&mut linker).map_err(failure)?;
        bindings::ProviderWorld::add_to_linker::<_, wasmtime::component::HasSelf<HostState>>(
            &mut linker,
            |state| state,
        )
        .map_err(failure)?;

        // Running code yields at every tick, so a busy or looping provider
        // never holds a runtime thread. The thread ends with the engine.
        let weak = engine.weak();
        std::thread::Builder::new()
            .name("violet-wasm-epoch".into())
            .spawn(move || {
                while let Some(engine) = weak.upgrade() {
                    engine.increment_epoch();
                    drop(engine);
                    std::thread::sleep(TICK);
                }
            })
            .map_err(|error| DbError::new(ErrorKind::Provider, error.to_string()))?;

        Ok(Self {
            engine,
            linker: Arc::new(linker),
        })
    }

    /// Compiles a provider and checks it fits the contract. Takes a while
    /// for a large component (unless cached); run it off async threads.
    pub fn load(&self, wasm: &[u8]) -> Result<Provider> {
        let component = Component::new(&self.engine, wasm).map_err(|error| {
            DbError::new(
                ErrorKind::Provider,
                format!("Not a valid provider component: {error}"),
            )
        })?;
        let pre = self.linker.instantiate_pre(&component).map_err(|error| {
            DbError::new(
                ErrorKind::Provider,
                format!(
                    "The provider needs something Violet does not offer (built for another \
                     version of the provider API?): {error}"
                ),
            )
        })?;
        let pre = ProviderWorldPre::new(pre).map_err(|error| {
            DbError::new(
                ErrorKind::Provider,
                format!("The provider does not export the provider API {API_VERSION}: {error}"),
            )
        })?;
        Ok(Provider {
            engine: self.engine.clone(),
            pre,
        })
    }

    /// Compiles a provider and asks it to describe itself.
    pub async fn inspect(&self, wasm: Vec<u8>) -> Result<Description> {
        let runtime = self.clone();
        let provider = tokio::task::spawn_blocking(move || runtime.load(&wasm))
            .await
            .map_err(failure)??;
        provider.describe().await
    }
}

/// A compiled provider, ready to instantiate.
pub struct Provider {
    engine: Engine,
    pre: ProviderWorldPre<HostState>,
}

impl Provider {
    /// The provider's own description, checked.
    pub async fn describe(&self) -> Result<Description> {
        let mut instance = session::Instance::start(self, None).await?;
        let info = instance.info().await?;
        convert::description(info)
    }
}

/// What a provider says about itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Description {
    pub info: DriverInfo,
    pub version: String,
    pub description: String,
}

fn failure(error: impl std::fmt::Display) -> DbError {
    DbError::new(ErrorKind::Provider, error.to_string())
}
