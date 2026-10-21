//! Languages Violet speaks besides SQL.
//!
//! The editor needs answers on every frame (colours, the statement under
//! the cursor, whether a statement writes or carries a password), so the
//! app knows each language itself. Providers depend on this crate too, so
//! both cut a script into the same statements.
//!
//! No I/O and no dependencies: it builds for the app and for
//! `wasm32-wasip2`.

pub mod redis;
