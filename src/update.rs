//! Self-update from Violet's GitHub releases, through fastframe-update.
//!
//! A worker thread checks once at start-up and then daily, or right away
//! when the user retries. When a newer release exists and this copy may
//! replace itself (a copy the installer or the portable archive put there,
//! not one a package manager owns), it downloads and verifies it against
//! the signed `checksums.txt`; the interface then offers to restart into
//! it. Handing the update over runs off the interface thread, and once the
//! helper has it the window closes and the process exits at once (see
//! [`handed_off`]). Debug builds never check.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::Duration;

use fastframe_update::{
    MacConfig, Prepared, Request, Response, Transport, Unsupported, UpdateConfig, Updater,
};

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
    /// Handing the update to the helper; the window closes once it has it.
    Installing { version: String },
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
    /// Asks the worker to check again now.
    wake: bool,
    /// The helper has the update: the app must quit.
    quit: bool,
}

/// What the worker and the interface share.
struct State {
    shared: Mutex<Shared>,
    /// Signalled with `wake`.
    woken: Condvar,
}

/// Set once the helper has taken an update. The helper waits for this
/// process to exit, so `main` exits right after the window closes instead
/// of waiting on anything still shutting down.
static HANDED_OFF: AtomicBool = AtomicBool::new(false);

/// Whether an update was handed to the helper, which waits for Violet to
/// exit before installing it.
pub fn handed_off() -> bool {
    HANDED_OFF.load(Ordering::SeqCst)
}

/// The update worker and what it found.
pub struct Updates {
    updater: Updater,
    state: Arc<State>,
    repaint: Arc<dyn Fn() + Send + Sync>,
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
        let state = Arc::new(State {
            shared: Mutex::new(Shared {
                status: Status::Idle,
                prepared: None,
                wake: false,
                quit: false,
            }),
            woken: Condvar::new(),
        });
        let repaint: Arc<dyn Fn() + Send + Sync> = Arc::new(repaint);
        let worker = (updater.clone(), Arc::clone(&state), Arc::clone(&repaint));
        let spawned = std::thread::Builder::new()
            .name("updates".into())
            .spawn(move || {
                let (updater, state, repaint) = worker;
                // Let the window come up first.
                std::thread::sleep(Duration::from_secs(5));
                loop {
                    let ready = cycle(&updater, &state.shared, &*repaint);
                    // Downloaded: nothing more to do until a restart, unless
                    // handing it over fails and the user tries again.
                    let wait = if ready {
                        Duration::from_secs(365 * 24 * 60 * 60)
                    } else {
                        fastframe_update::CHECK_INTERVAL
                    };
                    state.sleep(wait);
                }
            });
        if let Err(error) = spawned {
            log::error!("updates: {error}");
            return None;
        }
        Some(Self {
            updater,
            state,
            repaint,
        })
    }

    pub fn status(&self) -> Status {
        lock(&self.state.shared).status.clone()
    }

    /// True once the helper has the update: close the window, and `main`
    /// exits so the helper can install it.
    pub fn should_quit(&self) -> bool {
        lock(&self.state.shared).quit
    }

    /// Checks, and downloads, again now: after a failure.
    pub fn retry(&self) {
        let mut shared = lock(&self.state.shared);
        shared.status = Status::Idle;
        shared.wake = true;
        drop(shared);
        self.state.woken.notify_all();
        (self.repaint)();
    }

    /// Hands the downloaded update to the helper, off the interface thread:
    /// the helper is a copy of Violet and takes a moment to start. Once it
    /// has the update, [`Updates::should_quit`] turns true; the helper
    /// installs it after Violet exits, then starts the new version.
    pub fn restart(&self) -> anyhow::Result<()> {
        let mut shared = lock(&self.state.shared);
        let Status::Ready { version } = shared.status.clone() else {
            anyhow::bail!("No update is ready. Try again.");
        };
        let prepared = shared
            .prepared
            .take()
            .ok_or_else(|| anyhow::anyhow!("No update is ready. Try again."))?;
        shared.status = Status::Installing {
            version: version.clone(),
        };
        drop(shared);
        (self.repaint)();
        let (updater, state, repaint) = (
            self.updater.clone(),
            Arc::clone(&self.state),
            Arc::clone(&self.repaint),
        );
        let spawned = std::thread::Builder::new()
            .name("update-handoff".into())
            .spawn(move || {
                let handed = updater.handoff(prepared, Vec::new());
                let mut shared = lock(&state.shared);
                match handed {
                    Ok(()) => {
                        log::info!("updates: the helper has Violet {version}; quitting");
                        HANDED_OFF.store(true, Ordering::SeqCst);
                        shared.quit = true;
                    }
                    Err(error) => {
                        log::error!("updates: could not hand over {version}: {error:#}");
                        shared.status = Status::Failed {
                            url: release_page(&version),
                            version,
                            error: format!("{error:#}"),
                        };
                    }
                }
                drop(shared);
                repaint();
            });
        if let Err(error) = spawned {
            // The update went with the thread: download it again.
            self.retry();
            return Err(error.into());
        }
        Ok(())
    }
}

impl State {
    /// Waits for `timeout`, or until [`Updates::retry`] wakes it.
    fn sleep(&self, timeout: Duration) {
        let shared = lock(&self.shared);
        let (mut shared, _) = self
            .woken
            .wait_timeout_while(shared, timeout, |shared| !shared.wake)
            .unwrap_or_else(PoisonError::into_inner);
        shared.wake = false;
    }
}

fn lock(shared: &Mutex<Shared>) -> std::sync::MutexGuard<'_, Shared> {
    shared.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The release page of `version`.
fn release_page(version: &str) -> String {
    format!(
        "https://github.com/{}/releases/tag/v{version}",
        UPDATES.repository
    )
}

/// Why this copy cannot update itself, in Violet's words.
fn manual_reason(reason: &Unsupported) -> String {
    match reason {
        Unsupported::NotPortable if cfg!(windows) => {
            "This copy cannot update itself: it was not installed with Violet's installer. \
             Install the .msi from the release page once, and Violet keeps itself up to date \
             from then on."
                .into()
        }
        Unsupported::NotPortable => {
            "This copy cannot update itself: it is not in the folder it was unpacked in. \
             Download the new version from the release page."
                .into()
        }
        reason => reason.to_string(),
    }
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
        log::info!("updates: this copy cannot update itself: {reason}");
        set(Status::Manual {
            version: release.version.clone(),
            url: release.url.clone(),
            reason: manual_reason(&reason),
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
