//! Timing a connection's round trips, to split a run's time with and to
//! tell when a connection stops answering.

use std::time::{Duration, Instant};

use violet_app::{CachePolicy, ExecOptions};
use violet_core::{ConnectionId, ErrorKind, QueryOutput, Result};

use super::VioletApp;
use super::state::Link;
use super::toasts::Toast;

/// A ping older than this is measured again.
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

    /// Pings `connection` when its last ping is old.
    pub(super) fn ping_if_stale(&mut self, connection: ConnectionId) {
        let stale = self
            .explorer
            .metrics
            .get(&connection)
            .is_none_or(|metrics| metrics.latency.stale(Instant::now(), STALE));
        if stale && self.backend.is_connected(connection) {
            self.ping(connection);
        }
    }

    /// Pings the idle connections whose last ping is old, so a dropped one
    /// shows in the status bar before the next query finds it.
    pub(super) fn ping_tick(&mut self, ctx: &egui::Context) {
        let online: Vec<ConnectionId> = self
            .explorer
            .links
            .iter()
            .filter(|(_, link)| matches!(link, Link::Online(_)))
            .map(|(id, _)| *id)
            .collect();
        if online.is_empty() {
            return;
        }
        ctx.request_repaint_after(Duration::from_secs(5));
        for id in online {
            let busy = self
                .workspace
                .tabs
                .iter()
                .any(|tab| tab.connection == Some(id) && tab.running.is_some());
            if !busy {
                self.ping_if_stale(id);
            }
        }
    }

    /// A ping answered, or did not.
    pub(super) fn pinged(
        &mut self,
        ctx: &egui::Context,
        connection: ConnectionId,
        result: &Result<QueryOutput>,
    ) {
        let online = self.explorer.link(connection).is_online();
        let metrics = self.explorer.metrics.entry(connection).or_default();
        match result {
            Ok(output) => metrics.ping_answered(output.elapsed, Instant::now()),
            Err(error) => {
                metrics.ping_failed();
                let lost = matches!(error.kind, ErrorKind::Connection | ErrorKind::NotConnected)
                    && !self.backend.is_connected(connection);
                if lost && online {
                    metrics.dropped();
                    self.explorer.links.insert(connection, Link::Offline);
                    self.explorer.forget(connection);
                    self.toasts.push(
                        ctx,
                        Toast::error(
                            format!("Lost the connection to {}", self.profile_name(connection)),
                            error.message.clone(),
                        ),
                    );
                }
            }
        }
    }
}
