//! Timing a connection's round trips, to split a run's time with.

use std::time::{Duration, Instant};

use violet_app::{CachePolicy, ExecOptions};
use violet_core::{ConnectionId, QueryOutput, Result};

use super::VioletApp;

/// A ping older than this is measured again after the next run.
const STALE: Duration = Duration::from_secs(30);

impl VioletApp {
    /// Times a round trip to `connection`.
    pub(super) fn ping(&mut self, connection: ConnectionId) {
        if self.ping_requests.values().any(|id| *id == connection) {
            return;
        }
        let statement = crate::lang::ping(self.language(connection));
        let options = ExecOptions {
            cache: CachePolicy::Refresh,
            page: None,
        };
        let request = self
            .backend
            .execute(connection, statement.to_owned(), options);
        self.ping_requests.insert(request, connection);
    }

    /// Pings `connection` when its last ping is old, after a run that left
    /// the session idle.
    pub(super) fn ping_if_stale(&mut self, connection: ConnectionId) {
        let stale = self
            .explorer
            .latency
            .get(&connection)
            .is_none_or(|latency| latency.stale(Instant::now(), STALE));
        if stale && self.backend.is_connected(connection) {
            self.ping(connection);
        }
    }

    /// A ping answered: its round trip is the output's time.
    pub(super) fn pinged(&mut self, connection: ConnectionId, result: &Result<QueryOutput>) {
        if let Ok(output) = result {
            self.explorer
                .latency
                .entry(connection)
                .or_default()
                .record(output.elapsed, Instant::now());
        }
    }
}
