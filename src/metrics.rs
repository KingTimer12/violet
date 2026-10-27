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
    fn goes_stale_with_age() {
        let mut latency = Latency::default();
        let start = Instant::now();
        latency.record(Duration::from_millis(5), start);
        let age = Duration::from_secs(30);
        assert!(!latency.stale(start + Duration::from_secs(10), age));
        assert!(latency.stale(start + Duration::from_secs(30), age));
    }
}
