//! Violet's application layer: what the interface asks for, and the events
//! that answer it.
//!
//! The interface never waits on a database. It calls [`Backend`] methods,
//! which start work on a private async runtime and return at once; results
//! arrive as [`Event`]s, and a [`Notify`] tells the interface to collect
//! them. Nothing here knows about egui or any particular database: drivers
//! come in through a [`DriverRegistry`], storage through the ports in
//! `violet_core::ports`.

mod backend;
mod profiles;
mod registry;

pub use backend::{Backend, Event, ExecOptions, Notify, RequestId};
pub use profiles::ProfileService;
pub use registry::DriverRegistry;
