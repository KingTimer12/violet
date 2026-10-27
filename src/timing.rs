//! Where the time of a run went: the network, the server, fetching the
//! rows and drawing them.

use std::time::Duration;

/// A part of a run's time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Part {
    /// A round trip to the server, as the last ping measured it.
    Network,
    /// Planning and running the statement, as an analyzed plan reports.
    Server,
    /// Reading the rows over the wire. Without an analyzed plan the server's
    /// time cannot be told from it, and they stay one part.
    Fetch,
    /// What the server does and sends back, together.
    ServerAndFetch,
    /// From the result reaching the app to the rows being on screen.
    Render,
}

impl Part {
    pub fn label(self) -> &'static str {
        match self {
            Self::Network => "Network",
            Self::Server => "Server",
            Self::Fetch => "Fetch",
            Self::ServerAndFetch => "Server + fetch",
            Self::Render => "Render",
        }
    }

    pub fn hint(self) -> &'static str {
        match self {
            Self::Network => {
                "One round trip, from the last ping. Slow here: the server is far away."
            }
            Self::Server => "Planning and execution, as Explain analyze measured them.",
            Self::Fetch => "The rest of the wait: sending the rows and reading them.",
            Self::ServerAndFetch => {
                "Everything past the round trip. Explain analyze splits the server's part out."
            }
            Self::Render => "From the result arriving to the rows being drawn.",
        }
    }
}

/// A run's time, split.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Breakdown {
    /// What the driver took, from sending the statement to having the rows.
    pub total: Duration,
    pub parts: Vec<(Part, Duration)>,
}

impl Breakdown {
    /// Splits `total`: `rtt` is the connection's last round trip, `server`
    /// what an analyzed plan reported, `render` the time to draw. Each part
    /// is kept within what the earlier ones left.
    pub fn new(
        total: Duration,
        rtt: Option<Duration>,
        server: Option<Duration>,
        render: Option<Duration>,
    ) -> Self {
        let mut parts = Vec::new();
        let mut left = total;
        let mut take = |part: Part, wanted: Duration, parts: &mut Vec<(Part, Duration)>| {
            let used = wanted.min(left);
            left -= used;
            parts.push((part, used));
        };
        if let Some(rtt) = rtt {
            take(Part::Network, rtt, &mut parts);
        }
        match server {
            Some(server) => {
                take(Part::Server, server, &mut parts);
                parts.push((Part::Fetch, left));
            }
            None => parts.push((Part::ServerAndFetch, left)),
        }
        if let Some(render) = render {
            parts.push((Part::Render, render));
        }
        Self { total, parts }
    }

    /// What the user waited: the driver's time and the drawing.
    pub fn felt(&self) -> Duration {
        self.parts.iter().map(|(_, took)| *took).sum()
    }

    /// The part that took the longest.
    pub fn slowest(&self) -> Option<Part> {
        self.parts
            .iter()
            .max_by_key(|(_, took)| *took)
            .map(|(part, _)| *part)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn splits_with_a_plan_and_a_ping() {
        let b = Breakdown::new(ms(100), Some(ms(10)), Some(ms(60)), Some(ms(5)));
        assert_eq!(
            b.parts,
            [
                (Part::Network, ms(10)),
                (Part::Server, ms(60)),
                (Part::Fetch, ms(30)),
                (Part::Render, ms(5)),
            ]
        );
        assert_eq!(b.felt(), ms(105));
        assert_eq!(b.slowest(), Some(Part::Server));
    }

    #[test]
    fn without_a_plan_server_and_fetch_stay_together() {
        let b = Breakdown::new(ms(40), Some(ms(10)), None, None);
        assert_eq!(
            b.parts,
            [(Part::Network, ms(10)), (Part::ServerAndFetch, ms(30))]
        );
        let none = Breakdown::new(ms(40), None, None, None);
        assert_eq!(none.parts, [(Part::ServerAndFetch, ms(40))]);
    }

    #[test]
    fn parts_never_exceed_the_total() {
        // A ping slower than the whole run, a plan longer than what is left.
        let b = Breakdown::new(ms(20), Some(ms(50)), Some(ms(30)), None);
        assert_eq!(
            b.parts,
            [
                (Part::Network, ms(20)),
                (Part::Server, Duration::ZERO),
                (Part::Fetch, Duration::ZERO),
            ]
        );
        assert_eq!(b.felt(), ms(20));
    }
}
