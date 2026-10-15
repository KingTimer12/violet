//! Self-update from Violet's GitHub releases, through fastframe-update.
//!
//! A worker thread checks once at start-up and then daily. When a newer
//! release exists and this copy may replace itself (a downloaded copy, not
//! one a package manager owns), it downloads and verifies it against the
//! signed `checksums.txt`; the interface then offers to restart into it.
//! Debug builds never check.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use fastframe_update::{MacConfig, Prepared, Request, Response, Transport, UpdateConfig, Updater};

/// Where releases come from and how they are signed. The public key's
/// private half signs `checksums.txt` in the release workflow.
pub const UPDATES: UpdateConfig = UpdateConfig {
    macos: MacConfig {
        bundle_ids: &["dev.violet.Violet"],
        executable_names: &[],
        legacy_bundle_names: &[],
    },
    publisher_key: Some(include_str!("../assets/update-public-key.hex")),
    ..UpdateConfig::new(
        "KingTimer12/violet",
        "Violet",
        "violet",
        env!("CARGO_PKG_VERSION"),
    )
};

/// What the interface shows about updates.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Status {
    /// Nothing newer, or not checked yet.
    #[default]
    Idle,
    Downloading {
        version: String,
        /// Whole percent of the download.
        percent: u8,
    },
    /// Downloaded and verified: restarting installs it.
    Ready { version: String },
    /// A newer release this copy cannot install itself, and why.
    Manual {
        version: String,
        url: String,
        reason: String,
    },
    Failed {
        version: String,
        url: String,
        error: String,
    },
}

struct Shared {
    status: Status,
    prepared: Option<Prepared>,
}

/// The update worker and what it found.
pub struct Updates {
    updater: Updater,
    shared: Arc<Mutex<Shared>>,
}

impl Updates {
    /// Starts checking in the background; `repaint` wakes the interface
    /// when the status changes. `None` in debug builds.
    pub fn start(repaint: impl Fn() + Send + Sync + 'static) -> Option<Self> {
        if cfg!(debug_assertions) {
            return None;
        }
        if let Err(error) = UPDATES.validate() {
            log::error!("updates: {error:#}");
            return None;
        }
        let updater = Updater::new(UPDATES, UreqTransport::new());
        let shared = Arc::new(Mutex::new(Shared {
            status: Status::Idle,
            prepared: None,
        }));
        let worker = (updater.clone(), Arc::clone(&shared));
        let spawned = std::thread::Builder::new()
            .name("updates".into())
            .spawn(move || {
                let (updater, shared) = worker;
                // Let the window come up first.
                std::thread::sleep(Duration::from_secs(5));
                loop {
                    if cycle(&updater, &shared, &repaint) {
                        // Downloaded: nothing more to do until a restart.
                        return;
                    }
                    std::thread::sleep(fastframe_update::CHECK_INTERVAL);
                }
            });
        if let Err(error) = spawned {
            log::error!("updates: {error}");
            return None;
        }
        Some(Self { updater, shared })
    }

    pub fn status(&self) -> Status {
        lock(&self.shared).status.clone()
    }

    /// Hands the downloaded update to the helper. Quit right after it
    /// returns: the helper installs once Violet has exited, then starts
    /// the new version.
    pub fn restart(&self) -> anyhow::Result<()> {
        let prepared = lock(&self.shared)
            .prepared
            .take()
            .ok_or_else(|| anyhow::anyhow!("No update is ready"))?;
        self.updater.handoff(prepared, Vec::new())
    }
}

fn lock(shared: &Mutex<Shared>) -> std::sync::MutexGuard<'_, Shared> {
    shared.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One check: finds a newer release and downloads it. True once an update
/// is ready.
fn cycle(updater: &Updater, shared: &Mutex<Shared>, repaint: &dyn Fn()) -> bool {
    let set = |status: Status| {
        lock(shared).status = status;
        repaint();
    };
    let release = match updater.check() {
        Ok(Some(release)) => release,
        Ok(None) => return false,
        Err(error) => {
            log::warn!("updates: could not check: {error:#}");
            return false;
        }
    };
    log::info!("updates: Violet {} is available", release.version);
    if let Err(reason) = updater.installation() {
        set(Status::Manual {
            version: release.version.clone(),
            url: release.url.clone(),
            reason: reason.to_string(),
        });
        return false;
    }
    set(Status::Downloading {
        version: release.version.clone(),
        percent: 0,
    });
    let mut shown = 0u8;
    let downloaded = updater.download(&release, |received, total| {
        let percent = received
            .saturating_mul(100)
            .checked_div(total)
            .map_or(0, |percent| u8::try_from(percent.min(100)).unwrap_or(100));
        if percent != shown {
            shown = percent;
            set(Status::Downloading {
                version: release.version.clone(),
                percent,
            });
        }
    });
    match downloaded {
        Ok(prepared) => {
            let mut shared = lock(shared);
            shared.status = Status::Ready {
                version: release.version.clone(),
            };
            shared.prepared = Some(prepared);
            drop(shared);
            repaint();
            true
        }
        Err(error) => {
            log::warn!("updates: could not download {}: {error:#}", release.version);
            set(Status::Failed {
                version: release.version.clone(),
                url: release.url.clone(),
                error: format!("{error:#}"),
            });
            false
        }
    }
}

/// The updater's HTTP client: ureq, as the provider downloads use, without
/// following redirects (the updater follows them itself, only to GitHub's
/// release hosts) and with every status returned as is.
struct UreqTransport(ureq::Agent);

impl UreqTransport {
    fn new() -> Self {
        Self(ureq::Agent::new_with_config(
            ureq::Agent::config_builder()
                .https_only(true)
                .max_redirects(0)
                .max_redirects_will_error(false)
                .http_status_as_error(false)
                .timeout_connect(Some(Duration::from_secs(15)))
                .timeout_global(Some(Duration::from_secs(15 * 60)))
                .build(),
        ))
    }
}

impl Transport for UreqTransport {
    fn get(&self, request: &Request<'_>) -> anyhow::Result<Response> {
        let response = self
            .0
            .get(request.url)
            .header("Accept", request.accept)
            .header("User-Agent", request.user_agent)
            .call()?;
        let status = response.status().as_u16();
        let location = response
            .headers()
            .get("Location")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let body: Box<dyn std::io::Read + Send> = Box::new(response.into_body().into_reader());
        Ok(Response {
            status,
            location,
            body,
        })
    }
}

/// Tells the helper the new version started, so it keeps it; off the
/// interface thread, after the first frame.
pub fn acknowledge(receipt: fastframe_update::Receipt) {
    let _ = std::thread::Builder::new()
        .name("update-receipt".into())
        .spawn(move || {
            if let Err(error) = receipt.acknowledge() {
                log::error!("updates: could not confirm the update: {error:#}");
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_configuration_is_valid() {
        UPDATES.validate().expect("valid update configuration");
    }
    /// Reads the latest release from GitHub as an older version would,
    /// following GitHub's answers through the transport.
    /// `cargo test -p violet update -- --ignored`
    #[test]
    #[ignore = "reaches api.github.com"]
    fn an_older_version_finds_the_latest_release() {
        let config = UpdateConfig {
            current_version: "0.0.1",
            ..UPDATES
        };
        let release = Updater::new(config, UreqTransport::new())
            .check()
            .expect("check")
            .expect("a newer release");
        assert!(
            release
                .url
                .starts_with("https://github.com/KingTimer12/violet/releases/")
        );
        assert!(fastframe_update::is_newer(&release.version, "0.0.1"));
    }
}
