//! Redis for Violet, as a WebAssembly provider: Redis 6 and later, and the
//! servers that speak its protocol (Valkey, KeyDB, Dragonfly, Garnet...).
//!
//! Speaks RESP over the socket Violet hands it, TLS done by Violet from
//! the first byte (`rediss://`). Databases `db0`, `db1`... are the tree's
//! schemas and keys its objects, listed with `SCAN` a page at a time.

mod render;
mod resp;
mod session;
mod tools;

use violet_sdk::data::{Language, Login, ProviderExtras};
use violet_sdk::tools::{Download, Task};
use violet_sdk::{Capabilities, ConnectParams, Provider, ProviderInfo, Result};

pub use session::RedisSession;

pub struct Redis;

impl Provider for Redis {
    type Session = RedisSession;

    fn info() -> ProviderInfo {
        ProviderInfo {
            description: "Redis 6 and later, Valkey, KeyDB, Dragonfly and compatible servers"
                .into(),
            default_database: "db0".into(),
            capabilities: Capabilities {
                databases: false,
                schemas: true,
                cancel: false,
                tls: true,
            },
            ..ProviderInfo::new("redis", "Redis", 6379)
        }
    }

    fn extras() -> ProviderExtras {
        ProviderExtras {
            language: Language::Redis,
            user: Login::Optional,
            password: Login::Optional,
            schema_label: "Databases".into(),
        }
    }

    fn connect(params: ConnectParams) -> Result<RedisSession> {
        RedisSession::connect(params)
    }

    fn tasks() -> Vec<Task> {
        tools::tasks()
    }

    fn downloads() -> Vec<Download> {
        tools::downloads()
    }
}

violet_sdk::export_provider!(Redis);
