//! Violet: a modern database client.
//!
//! This is the composition root. It builds the concrete adapters (the
//! provider store, storage), hands them to the application layer, and
//! starts the interface. Violet ships no database driver: each database is
//! a WebAssembly provider installed on demand (`violet-providers`).

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod completion;
mod settings;
mod sql;
mod ui;

use std::sync::Arc;

use violet_app::{Backend, ProfileService};
use violet_core::ports::{
    ProfileRepository, ProviderStore, QueryHistory, ResultCache, SecretStore,
};
use violet_providers::{DEFAULT_INDEX, Providers, Source};
use violet_storage::{
    AppDirs, JsonFile, JsonLinesHistory, JsonProfileRepository, KeyringSecretStore,
    MemoryResultCache,
};

use settings::Settings;
use ui::{Services, VioletApp};

#[hotpath::main]
fn main() -> eframe::Result {
    // `VIOLET_HOME` keeps everything under one directory: portable installs,
    // and runs that must not touch the user's own connections.
    let dirs = std::env::var_os("VIOLET_HOME")
        .map(AppDirs::at)
        .or_else(AppDirs::discover)
        .unwrap_or_else(|| AppDirs::at(".violet"));
    init_logging(&dirs);

    let viewport = egui::ViewportBuilder::default()
        .with_title("Violet")
        .with_app_id("dev.violet.Violet")
        .with_inner_size([1360.0, 860.0])
        .with_min_inner_size([880.0, 560.0])
        // Draw under the title bar on macOS; the header leaves room for the
        // traffic lights (fastframe-macos).
        .with_fullsize_content_view(true)
        .with_titlebar_shown(false)
        .with_title_shown(false);

    let options = eframe::NativeOptions {
        viewport,
        centered: true,
        ..Default::default()
    };

    eframe::run_native(
        "Violet",
        options,
        Box::new(move |cc| {
            let ctx = cc.egui_ctx.clone();
            let settings_file: JsonFile<Settings> = JsonFile::new(dirs.settings_file());
            let mut startup_errors = Vec::new();
            let settings = settings_file.read().unwrap_or_else(|error| {
                log::warn!("settings: {error}");
                None
            });
            #[cfg_attr(not(debug_assertions), allow(unused_mut))]
            let mut settings: Settings = settings.unwrap_or_default();
            #[cfg(debug_assertions)]
            if let Ok(theme) = std::env::var("VIOLET_THEME") {
                settings.theme = if theme == "light" {
                    settings::ThemeChoice::Light
                } else {
                    settings::ThemeChoice::Dark
                };
            }

            ui::theme::install(&ctx, settings.theme.into());
            egui_extras::install_image_loaders(&ctx);
            fastframe_icons::install::<ui::icons::Icon>(&ctx);

            let secrets: Arc<dyn SecretStore> = Arc::new(KeyringSecretStore);
            let repository: Arc<dyn ProfileRepository> =
                Arc::new(JsonProfileRepository::new(dirs.profiles_file()));
            let (profiles, error) = ProfileService::load(repository, Arc::clone(&secrets));
            if let Some(error) = error {
                log::error!("profiles: {error}");
                startup_errors.push(ui::toast_error(
                    "Saved connections could not be read",
                    error.to_string(),
                ));
            }

            // `VIOLET_PROVIDER_INDEX` points at another catalogue: a local
            // `providers.json` while developing a provider, or a mirror.
            let index = std::env::var("VIOLET_PROVIDER_INDEX")
                .ok()
                .and_then(|location| match Source::parse(&location) {
                    Ok(source) => Some(source),
                    Err(error) => {
                        startup_errors.push(ui::toast_error(
                            "VIOLET_PROVIDER_INDEX ignored",
                            error.to_string(),
                        ));
                        None
                    }
                })
                .map_or_else(|| Source::parse(DEFAULT_INDEX), Ok)?;
            let wasm = violet_wasm::Runtime::new(Some(&dirs.code_cache_dir()))?;
            let providers_dir = dirs.providers_dir();
            let providers: Arc<dyn ProviderStore> =
                Arc::new(Providers::new(providers_dir.clone(), wasm, index));
            let repaint = ctx.clone();
            // Read-only results are kept in memory only, never on disk.
            let cache: Arc<dyn ResultCache> = Arc::new(MemoryResultCache::default());
            let backend = Backend::start(providers, secrets, cache, move || {
                repaint.request_repaint();
            })?;

            let history: Arc<dyn QueryHistory> =
                Arc::new(JsonLinesHistory::new(dirs.history_file(), ui::HISTORY_SIZE));
            let services = Services {
                backend,
                profiles,
                settings: settings_file,
                providers_dir,
                history,
                session: JsonFile::new(dirs.session_file()),
            };
            Ok(Box::new(VioletApp::new(
                &ctx,
                services,
                settings,
                startup_errors,
            )))
        }),
    )
}

fn init_logging(dirs: &AppDirs) {
    let state = dirs.state_dir();
    let mut logging =
        fastframe_log::Logging::new("violet", env!("CARGO_PKG_VERSION")).filter("warn,violet=info");
    if std::fs::create_dir_all(state).is_ok() {
        logging = logging.file(dirs.log_file()).panic_log(dirs.panic_log());
    }
    // Only fails when a logger is already installed, which cannot happen
    // this early.
    let _ = logging.init();
}
