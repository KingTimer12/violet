//! MySQL and MariaDB for Violet, as a WebAssembly provider.
//!
//! Speaks the client/server protocol itself over the socket Violet hands
//! it; TLS is negotiated with the SSL request packet and done by Violet.
//! Authentication: `caching_sha2_password` (MySQL 8's default, with the
//! password RSA-encrypted when the connection is plain),
//! `mysql_native_password`, `sha256_password`, and `mysql_clear_password`
//! over TLS only.

mod auth;
mod catalog;
mod packet;
mod script;
mod session;
mod tools;
mod types;
mod wire;

use violet_sdk::tools::{Download, Task};
use violet_sdk::{Capabilities, ConnectParams, Dialect, Paging, Provider, ProviderInfo, Result};

pub use session::MySession;

pub struct MySql;

impl Provider for MySql {
    type Session = MySession;

    fn info() -> ProviderInfo {
        ProviderInfo {
            description: "MySQL 5.7 and later, MariaDB 10.3 and later".into(),
            default_user: "root".into(),
            capabilities: Capabilities {
                // A MySQL database is a schema: they show as schemas.
                databases: false,
                schemas: true,
                // Stopping a statement takes `KILL QUERY` from a second
                // authenticated connection, and cancelling runs without
                // the password.
                cancel: false,
                tls: true,
            },
            dialect: Dialect {
                quote_open: '`',
                quote_close: '`',
                paging: Paging::LimitOffset,
            },
            ..ProviderInfo::new("mysql", "MySQL", 3306)
        }
    }

    fn connect(params: ConnectParams) -> Result<MySession> {
        MySession::connect(&params)
    }

    fn tasks() -> Vec<Task> {
        tools::tasks()
    }

    fn downloads() -> Vec<Download> {
        tools::downloads()
    }
}

violet_sdk::export_provider!(MySql);
