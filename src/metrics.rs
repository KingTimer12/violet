//! What a connection's pings say about it.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Pings kept per connection.
const KEPT: usize = 30;

/// A connection's recent round trips.
#[derive(Clone, Debug, Default)]
pub struct Latency {
    samples: VecDeque<Duration>,
    at: Option<Instant>,
}

impl Latency {
    /// Notes a round trip that finished at `now`.
    pub fn record(&mut self, rtt: Duration, now: Instant) {
        if self.samples.len() == KEPT {
            self.samples.pop_front();
        }
        self.samples.push_back(rtt);
        self.at = Some(now);
    }

    /// The latest round trip.
    pub fn last(&self) -> Option<Duration> {
        self.samples.back().copied()
    }

    /// Whether no ping finished in the `max_age` before `now`.
    pub fn stale(&self, now: Instant, max_age: Duration) -> bool {
        self.at
            .is_none_or(|at| now.saturating_duration_since(at) >= max_age)
    }

    pub fn min(&self) -> Option<Duration> {
        self.samples.iter().min().copied()
    }

    pub fn max(&self) -> Option<Duration> {
        self.samples.iter().max().copied()
    }

    pub fn average(&self) -> Option<Duration> {
        let count = u32::try_from(self.samples.len()).ok().filter(|n| *n > 0)?;
        Some(self.samples.iter().sum::<Duration>() / count)
    }

    /// How much consecutive round trips differ, on average: a steady line
    /// has none, a flaky one a lot.
    pub fn jitter(&self) -> Option<Duration> {
        let gaps: Vec<Duration> = self
            .samples
            .iter()
            .zip(self.samples.iter().skip(1))
            .map(|(a, b)| a.abs_diff(*b))
            .collect();
        let count = u32::try_from(gaps.len()).ok().filter(|n| *n > 0)?;
        Some(gaps.iter().sum::<Duration>() / count)
    }
}

/// Pings in a row that went unanswered before a connection counts as not
/// answering.
const UNANSWERED: u32 = 2;

/// What is known of a connection's health, from connecting and pinging.
#[derive(Clone, Debug, Default)]
pub struct Metrics {
    pub latency: Latency,
    connecting_since: Option<Instant>,
    /// How long the last connect took.
    pub connect_time: Option<Duration>,
    online_since: Option<Instant>,
    /// Times a session was opened in this run.
    pub connects: u32,
    /// Times a session was lost without the user closing it.
    pub drops: u32,
    unanswered: u32,
}

impl Metrics {
    /// A connect starts at `now`.
    pub fn connecting(&mut self, now: Instant) {
        self.connecting_since = Some(now);
    }

    /// The session opened at `now`.
    pub fn connected(&mut self, now: Instant) {
        self.connect_time = self
            .connecting_since
            .take()
            .map(|since| now.saturating_duration_since(since));
        self.online_since = Some(now);
        self.connects += 1;
        self.unanswered = 0;
        self.latency = Latency::default();
    }

    /// The connect failed.
    pub fn connect_failed(&mut self) {
        self.connecting_since = None;
    }

    /// The user closed the session.
    pub fn closed(&mut self) {
        self.online_since = None;
        self.unanswered = 0;
    }

    /// The session was lost.
    pub fn dropped(&mut self) {
        self.closed();
        self.drops += 1;
    }

    pub fn ping_answered(&mut self, rtt: Duration, now: Instant) {
        self.unanswered = 0;
        self.latency.record(rtt, now);
    }

    pub fn ping_failed(&mut self) {
        self.unanswered += 1;
    }

    /// Pings keep failing though the session is still open.
    pub fn not_answering(&self) -> bool {
        self.unanswered >= UNANSWERED
    }

    /// Sessions opened after the first one.
    pub fn reconnects(&self) -> u32 {
        self.connects.saturating_sub(1)
    }

    /// How long the session has been open.
    pub fn online_for(&self, now: Instant) -> Option<Duration> {
        Some(now.saturating_duration_since(self.online_since?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_the_latest_round_trips() {
        let mut latency = Latency::default();
        let start = Instant::now();
        assert_eq!(latency.last(), None);
        assert!(latency.stale(start, Duration::from_secs(30)));
        for n in 1..=40 {
            latency.record(Duration::from_millis(n), start);
        }
        assert_eq!(latency.last(), Some(Duration::from_millis(40)));
        assert_eq!(latency.samples.len(), KEPT);
    }

    #[test]
    fn summarizes_round_trips() {
        let mut latency = Latency::default();
        let now = Instant::now();
        assert_eq!(
            (latency.min(), latency.average(), latency.jitter()),
            (None, None, None)
        );
        for ms in [10, 30, 20] {
            latency.record(Duration::from_millis(ms), now);
        }
        assert_eq!(latency.min(), Some(Duration::from_millis(10)));
        assert_eq!(latency.max(), Some(Duration::from_millis(30)));
        assert_eq!(latency.average(), Some(Duration::from_millis(20)));
        // Gaps of 20 and 10.
        assert_eq!(latency.jitter(), Some(Duration::from_millis(15)));
    }

    #[test]
    fn tracks_connects_drops_and_unanswered_pings() {
        let mut metrics = Metrics::default();
        let start = Instant::now();
        metrics.connecting(start);
        metrics.connected(start + Duration::from_millis(80));
        assert_eq!(metrics.connect_time, Some(Duration::from_millis(80)));
        assert_eq!(metrics.reconnects(), 0);
        assert_eq!(
            metrics.online_for(start + Duration::from_secs(5)),
            Some(Duration::from_millis(4920))
        );
        metrics.ping_answered(Duration::from_millis(7), start);
        assert!(!metrics.not_answering());
        metrics.ping_failed();
        assert!(!metrics.not_answering());
        metrics.ping_failed();
        assert!(metrics.not_answering());
        // An answer clears it.
        metrics.ping_answered(Duration::from_millis(9), start);
        assert!(!metrics.not_answering());
        metrics.dropped();
        assert_eq!((metrics.drops, metrics.online_for(start)), (1, None));
        metrics.connecting(start);
        metrics.connected(start + Duration::from_millis(30));
        assert_eq!((metrics.reconnects(), metrics.connects), (1, 2));
        // A fresh session starts its round trips over.
        assert_eq!(metrics.latency.last(), None);
        metrics.connecting(start);
        metrics.connect_failed();
        assert_eq!(metrics.connect_time, Some(Duration::from_millis(30)));
    }

    #[test]
    fn goes_stale_with_age() {
        let mut latency = Latency::default();
        let start = Instant::now();
        latency.record(Duration::from_millis(5), start);
        let age = Duration::from_secs(30);
        assert!(!latency.stale(start + Duration::from_secs(10), age));
        assert!(latency.stale(start + Duration::from_secs(30), age));
    }
}
