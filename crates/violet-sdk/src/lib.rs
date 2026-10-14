//! Build database providers for Violet.
//!
//! Violet does not ship database drivers. Each database is a *provider*: a
//! WebAssembly component Violet downloads into its `providers` folder the
//! first time a connection needs it, and runs in a sandbox. A provider sees
//! no files, no environment and no network except one stream to the server
//! the user asked to connect to (see [`net`]), so a faulty or hostile one
//! cannot reach anything else.
//!
//! A provider implements [`Provider`] and [`Session`] and registers them
//! with [`export_provider!`]:
//!
//! ```ignore
//! use violet_sdk::{prelude::*, net::Connection};
//!
//! struct Lite;
//!
//! impl Provider for Lite {
//!     type Session = LiteSession;
//!
//!     fn info() -> ProviderInfo {
//!         ProviderInfo::new("lite", "LiteDB", 7000)
//!     }
//!
//!     fn connect(params: ConnectParams) -> Result<LiteSession> {
//!         let connection = Connection::open()?;
//!         // ... handshake ...
//!         Ok(LiteSession { connection: RefCell::new(connection) })
//!     }
//! }
//!
//! impl Session for LiteSession { /* ... */ }
//!
//! violet_sdk::export_provider!(Lite);
//! ```
//!
//! Build it as a component with `cargo build --release --target
//! wasm32-wasip2`, then `violet-provider package` writes the index entry
//! Violet installs it from. The contract itself is `wit/provider.wit`, for
//! providers written in other languages.

mod error;
pub mod net;
mod rows;
pub mod tools;

#[doc(hidden)]
pub mod bindings {
    wit_bindgen::generate!({
        path: "wit",
        world: "provider-world",
        pub_export_macro: true,
        export_macro_name: "__export_provider_world",
        default_bindings_module: "violet_sdk::bindings",
    });
}

pub use bindings::violet::provider::types::{
    Capabilities, ColumnInfo, ColumnMeta, ConnectParams, DbError, DbObject, Dialect, ErrorKind,
    ObjectKind, Paging, ProviderInfo, ResultSet, Rows, ServerInfo, SslMode,
};
pub use rows::RowsBuilder;

/// The version of the provider contract this SDK speaks, as in
/// `wit/provider.wit`. Violet refuses providers built for another one.
pub const API_VERSION: &str = "0.1.0";

pub type Result<T, E = DbError> = std::result::Result<T, E>;

/// Everything a provider usually needs.
pub mod prelude {
    pub use std::cell::RefCell;

    pub use crate::{
        Capabilities, ColumnInfo, ColumnMeta, ConnectParams, DbError, DbObject, Dialect, ErrorKind,
        ObjectKind, Paging, Provider, ProviderInfo, Result, ResultSet, Rows, RowsBuilder,
        ServerInfo, Session, SslMode,
    };
}

/// A database provider: its description, and how to open sessions.
pub trait Provider: 'static {
    type Session: Session;

    /// Describes the provider. Must not touch the network.
    fn info() -> ProviderInfo;

    /// Opens a session. `params.ssl` is the mode to honour: Violet has
    /// already turned `Prefer` into `Require` for remote hosts.
    fn connect(params: ConnectParams) -> Result<Self::Session>;

    /// Backups and restores this provider runs through the database's
    /// client programs (see [`tools`]); none by default.
    fn tasks() -> Vec<tools::Task> {
        Vec::new()
    }

    /// The command line for one of [`Provider::tasks`]. `request.params`
    /// carries no password: name the variable for it with
    /// [`tools::ToolCommand::password_env`]. Violet checks the command
    /// runs one of the task's programs and shows it before it runs.
    fn command(kind: tools::TaskKind, request: tools::Request) -> Result<tools::ToolCommand> {
        let _ = (kind, request);
        Err(DbError::unsupported("Backup and restore are not supported"))
    }

    /// Settings that suit restoring from a path, such as its format, from a
    /// file's first bytes (`head`) or a folder's file names (`entries`).
    fn inspect(head: &[u8], entries: &[String], directory: bool) -> Vec<(String, String)> {
        let _ = (head, entries, directory);
        Vec::new()
    }

    /// Stops the statement the session that issued `ticket` (see
    /// [`Session::cancel_ticket`]) is running. Runs in a fresh instance,
    /// so it shares nothing with that session but the ticket.
    fn cancel(params: ConnectParams, ticket: Vec<u8>) -> Result<()> {
        let _ = (params, ticket);
        Err(DbError::unsupported("Cancelling is not supported"))
    }
}

/// An open connection.
///
/// Methods take `&self`, as the component model hands resources out
/// shared; keep mutable state in a [`std::cell::RefCell`]. Calls never
/// overlap: each session runs in its own instance, one call at a time.
pub trait Session: 'static {
    fn server_info(&self) -> Result<ServerInfo>;

    /// Other databases on the same server, for providers whose
    /// [`Capabilities::databases`] is set.
    fn databases(&self) -> Result<Vec<String>> {
        Ok(Vec::new())
    }

    fn schemas(&self) -> Result<Vec<String>>;

    fn objects(&self, schema: &str) -> Result<Vec<DbObject>>;

    fn columns(&self, schema: &str, relation: &str) -> Result<Vec<ColumnInfo>>;

    /// Runs one or more statements and returns one set per statement,
    /// keeping at most `max_rows` rows of each (see
    /// [`RowsBuilder::is_full`]).
    fn execute(&self, sql: &str, max_rows: usize) -> Result<Vec<ResultSet>>;

    /// A value that changes whenever data or schema may have changed, so
    /// Violet can tell whether a cached result is still good. `None` when
    /// the database has no such signal.
    fn change_token(&self) -> Result<Option<String>> {
        Ok(None)
    }

    /// Whatever [`Provider::cancel`] needs to stop this session's running
    /// statement, such as PostgreSQL's backend key. Asked once, after
    /// connecting.
    fn cancel_ticket(&self) -> Option<Vec<u8>> {
        None
    }
}

impl ProviderInfo {
    /// A provider with ANSI quoting, `LIMIT`/`OFFSET` paging, schemas and
    /// TLS; adjust the fields that differ.
    pub fn new(id: &str, name: &str, default_port: u16) -> Self {
        Self {
            id: id.to_owned(),
            name: name.to_owned(),
            version: String::new(),
            description: String::new(),
            default_port,
            default_database: String::new(),
            default_user: String::new(),
            capabilities: Capabilities {
                databases: false,
                schemas: true,
                cancel: false,
                tls: true,
            },
            dialect: Dialect::ANSI,
        }
    }
}

impl Dialect {
    /// Double-quoted identifiers and `LIMIT`/`OFFSET`.
    pub const ANSI: Self = Self {
        quote_open: '"',
        quote_close: '"',
        paging: Paging::LimitOffset,
    };
}

impl ConnectParams {
    /// A provider specific option from the profile.
    #[must_use]
    pub fn option(&self, key: &str) -> Option<&str> {
        self.options
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
}

impl ResultSet {
    /// A statement that returned rows.
    #[must_use]
    pub fn query(columns: Vec<ColumnMeta>, rows: RowsBuilder) -> Self {
        let truncated = rows.truncated();
        let count = rows.len() as u64;
        Self {
            columns,
            rows: rows.finish(),
            affected: Some(count),
            command: None,
            truncated,
        }
    }

    /// A command that returned no rows, such as `CREATE TABLE`.
    #[must_use]
    pub fn command(tag: impl Into<String>, affected: Option<u64>) -> Self {
        Self {
            columns: Vec::new(),
            rows: Rows {
                text: String::new(),
                ends: Vec::new(),
            },
            affected,
            command: Some(tag.into()),
            truncated: false,
        }
    }
}

impl ColumnMeta {
    #[must_use]
    pub fn new(name: impl Into<String>, type_name: Option<String>, numeric: bool) -> Self {
        Self {
            name: name.into(),
            type_name,
            numeric,
        }
    }
}

/// Writes to Violet's log. [`export_provider!`] installs it as the `log`
/// crate's logger, so `log::info!` and friends work in a provider.
pub mod logging {
    use crate::bindings::violet::provider::logging::{Level, log};

    struct HostLogger;

    impl ::log::Log for HostLogger {
        fn enabled(&self, metadata: &::log::Metadata<'_>) -> bool {
            metadata.level() <= ::log::Level::Debug
        }

        fn log(&self, record: &::log::Record<'_>) {
            if !self.enabled(record.metadata()) {
                return;
            }
            let level = match record.level() {
                ::log::Level::Error => Level::Error,
                ::log::Level::Warn => Level::Warn,
                ::log::Level::Info => Level::Info,
                ::log::Level::Debug | ::log::Level::Trace => Level::Debug,
            };
            log(level, &record.args().to_string());
        }

        fn flush(&self) {}
    }

    /// Routes `log` records and panic messages to Violet. Idempotent.
    pub fn install() {
        static LOGGER: HostLogger = HostLogger;
        if ::log::set_logger(&LOGGER).is_ok() {
            ::log::set_max_level(::log::LevelFilter::Debug);
            std::panic::set_hook(Box::new(|panic| {
                log(Level::Error, &format!("provider panicked: {panic}"));
            }));
        }
    }
}

/// The glue [`export_provider!`] expands to. Not a stable API.
#[doc(hidden)]
pub mod __private {
    pub use crate::bindings::exports::violet::provider::provider::{
        Guest, GuestSession, Session as SessionHandle,
    };
    pub use crate::bindings::exports::violet::provider::tools::Guest as ToolsGuest;

    use crate::{
        ColumnInfo, ConnectParams, DbError, DbObject, Provider, ProviderInfo, ResultSet,
        ServerInfo, Session,
    };

    /// Lets any [`Session`] stand behind the exported resource.
    pub struct SessionAdapter<S>(pub S);

    impl<S: Session> GuestSession for SessionAdapter<S> {
        fn server_info(&self) -> Result<ServerInfo, DbError> {
            self.0.server_info()
        }

        fn databases(&self) -> Result<Vec<String>, DbError> {
            self.0.databases()
        }

        fn schemas(&self) -> Result<Vec<String>, DbError> {
            self.0.schemas()
        }

        fn objects(&self, schema: String) -> Result<Vec<DbObject>, DbError> {
            self.0.objects(&schema)
        }

        fn columns(&self, schema: String, relation: String) -> Result<Vec<ColumnInfo>, DbError> {
            self.0.columns(&schema, &relation)
        }

        fn execute(&self, sql: String, max_rows: u32) -> Result<Vec<ResultSet>, DbError> {
            self.0.execute(&sql, max_rows as usize)
        }

        fn change_token(&self) -> Result<Option<String>, DbError> {
            self.0.change_token()
        }

        fn cancel_ticket(&self) -> Option<Vec<u8>> {
            self.0.cancel_ticket()
        }
    }

    pub fn info<P: Provider>(crate_version: &str) -> ProviderInfo {
        crate::logging::install();
        let mut info = P::info();
        if info.version.is_empty() {
            crate_version.clone_into(&mut info.version);
        }
        info
    }

    pub fn connect<P: Provider>(params: ConnectParams) -> Result<SessionHandle, DbError> {
        crate::logging::install();
        P::connect(params).map(|session| SessionHandle::new(SessionAdapter(session)))
    }

    pub fn cancel<P: Provider>(params: ConnectParams, ticket: Vec<u8>) -> Result<(), DbError> {
        crate::logging::install();
        P::cancel(params, ticket)
    }
}

/// Exports a [`Provider`] as the component's `provider` interface. Use it
/// once, at the root of the provider crate.
#[macro_export]
macro_rules! export_provider {
    ($provider:ty) => {
        #[doc(hidden)]
        pub struct __VioletProvider;

        impl $crate::__private::Guest for __VioletProvider {
            type Session =
                $crate::__private::SessionAdapter<<$provider as $crate::Provider>::Session>;

            fn info() -> $crate::ProviderInfo {
                $crate::__private::info::<$provider>(env!("CARGO_PKG_VERSION"))
            }

            fn connect(
                params: $crate::ConnectParams,
            ) -> ::std::result::Result<$crate::__private::SessionHandle, $crate::DbError> {
                $crate::__private::connect::<$provider>(params)
            }

            fn cancel(
                params: $crate::ConnectParams,
                ticket: ::std::vec::Vec<u8>,
            ) -> ::std::result::Result<(), $crate::DbError> {
                $crate::__private::cancel::<$provider>(params, ticket)
            }
        }

        impl $crate::__private::ToolsGuest for __VioletProvider {
            fn tasks() -> ::std::vec::Vec<$crate::tools::Task> {
                $crate::logging::install();
                <$provider as $crate::Provider>::tasks()
            }

            fn command(
                kind: $crate::tools::TaskKind,
                request: $crate::tools::Request,
            ) -> ::std::result::Result<$crate::tools::ToolCommand, $crate::DbError> {
                $crate::logging::install();
                <$provider as $crate::Provider>::command(kind, request)
            }

            fn inspect(
                head: ::std::vec::Vec<u8>,
                entries: ::std::vec::Vec<::std::string::String>,
                directory: bool,
            ) -> ::std::vec::Vec<(::std::string::String, ::std::string::String)> {
                $crate::logging::install();
                <$provider as $crate::Provider>::inspect(&head, &entries, directory)
            }
        }

        // Only a component has the host to export to; leaving the exports
        // out elsewhere lets a provider's unit tests run natively.
        #[cfg(target_family = "wasm")]
        $crate::bindings::__export_provider_world!(__VioletProvider with_types_in $crate::bindings);
    };
}
