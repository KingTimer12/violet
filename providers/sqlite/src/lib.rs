//! SQLite for Violet, as a WebAssembly provider.
//!
//! SQLite itself, compiled into the component, on a file layer of its own
//! ([`vfs`]) that reaches the database through Violet's `files` interface:
//! the provider sees the file the user picked, its journals, and nothing
//! else. Locks are SQLite's, on the same bytes native SQLite locks, so the
//! database can stay open in other programs.

mod db;
mod session;
mod vfs;

use violet_sdk::{Capabilities, ConnectParams, Provider, ProviderInfo, Result};

pub use session::LiteSession;

pub struct Sqlite;

impl Provider for Sqlite {
    type Session = LiteSession;

    fn info() -> ProviderInfo {
        ProviderInfo {
            description: "SQLite database files".into(),
            capabilities: Capabilities {
                // Attached databases show as schemas: `main`, `temp`...
                databases: false,
                schemas: true,
                cancel: false,
                tls: false,
            },
            // Port 0: a database file, which the profile's host names.
            ..ProviderInfo::new("sqlite", "SQLite", 0)
        }
    }

    fn connect(params: ConnectParams) -> Result<LiteSession> {
        LiteSession::connect(&params)
    }
}

violet_sdk::export_provider!(Sqlite);
