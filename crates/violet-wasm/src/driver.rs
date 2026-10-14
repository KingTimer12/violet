//! A provider as a Violet driver.

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::OnceCell;
use violet_core::{
    ConnectionProfile, Credentials, DbError, Driver, DriverInfo, ErrorKind, Result, Session,
};

use crate::session::WasmSession;
use crate::socket::Target;
use crate::{Provider, Runtime, convert};

/// Reads a provider's component, checking it is the one installed.
pub type Loader = Box<dyn Fn() -> Result<Vec<u8>> + Send + Sync>;

/// A driver backed by a provider component, compiled the first time a
/// connection needs it.
pub struct WasmDriver {
    info: DriverInfo,
    runtime: Runtime,
    loader: Loader,
    provider: OnceCell<Arc<Provider>>,
}

impl WasmDriver {
    /// A driver described by `info` (as recorded at install time) whose
    /// component `loader` reads.
    #[must_use]
    pub fn new(runtime: Runtime, info: DriverInfo, loader: Loader) -> Self {
        Self {
            info,
            runtime,
            loader,
            provider: OnceCell::new(),
        }
    }

    #[hotpath::measure]
    async fn provider(&self) -> Result<Arc<Provider>> {
        self.provider
            .get_or_try_init(|| async {
                let wasm = (self.loader)()?;
                let runtime = self.runtime.clone();
                let provider = tokio::task::spawn_blocking(move || runtime.load(&wasm))
                    .await
                    .map_err(|error| DbError::new(ErrorKind::Provider, error.to_string()))??;
                let actual = provider.describe().await?.info;
                if actual.id != self.info.id {
                    return Err(DbError::new(
                        ErrorKind::Provider,
                        format!(
                            "The installed \"{}\" provider says it is \"{}\"; reinstall it",
                            self.info.id, actual.id
                        ),
                    ));
                }
                Ok(Arc::new(provider))
            })
            .await
            .cloned()
    }
}

#[async_trait]
impl Driver for WasmDriver {
    fn info(&self) -> DriverInfo {
        self.info.clone()
    }

    async fn connect(
        &self,
        profile: &ConnectionProfile,
        credentials: &Credentials,
    ) -> Result<Box<dyn Session>> {
        let provider = self.provider().await?;
        let target = Target {
            host: profile.host.trim().to_owned(),
            port: profile.port,
            tls_required: convert::ssl(profile) == violet_core::SslMode::Require,
        };
        let params = convert::connect_params(profile, Some(credentials));
        let session = WasmSession::open(
            provider,
            target,
            params,
            self.info.dialect,
            self.info.capabilities.cancel,
        )
        .await?;
        log::info!(
            target: "violet::provider",
            "connected with {} ({:?})",
            self.info.id,
            convert::ssl(profile)
        );
        Ok(Box::new(session))
    }
}
