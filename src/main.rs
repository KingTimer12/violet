//! Violet: a modern database client.
//!
//! This is the composition root. It builds the concrete adapters (the
//! provider store, storage), hands them to the application layer, and
//! starts the interface. Violet ships no database driver: each database is
//! a WebAssembly provider installed on demand (`violet-providers`).

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod agent;
mod ai;
mod compare;
mod completion;
mod diagnostics;
mod dialect;
mod editing;
mod export;
mod format;
mod geometry;
mod hints;
mod lang;
mod mcp;
mod metrics;
mod mongo;
mod plan;
mod settings;
mod sql;
mod timing;
mod ui;
mod update;

use std::cell::RefCell;
use std::sync::Arc;

use violet_app::{Backend, ProfileService};
use violet_core::ports::{
    ProfileRepository, ProviderStore, QueryHistory, ResultCache, SecretStore,
};
use violet_providers::{DEFAULT_INDEX, Providers, Source};
use violet_storage::{
    AppDirs, JsonFile, JsonLinesHistory, JsonProfileRepository, KeyringSecretStore,
    MemoryResultCache, ScriptStore,
};

use settings::Settings;
use ui::{Services, VioletApp};

#[hotpath::main]
fn main() -> eframe::Result {
    // First: run as the update helper when asked to, and take the update
    // flags off the command line.
    let launch = fastframe_update::intercept(&update::UPDATES);
    // The helper of the previous version checks a download this way
    // before installing it.
    if launch
        .arguments
        .get(1)
        .is_some_and(|arg| arg == "--version")
    {
        #[allow(clippy::print_stdout)]
        {
            println!("violet {}", env!("CARGO_PKG_VERSION"));
        }
        return Ok(());
    }

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
        // The title bar, taskbar and Alt+Tab icon on Windows and Linux;
        // macOS takes the bundle's.
        .with_icon(
            eframe::icon_data::from_png_bytes(include_bytes!("../assets/app/violet.png"))
                .unwrap_or_default(),
        )
        // Draw under the title bar on macOS; the header leaves room for the
        // traffic lights (fastframe-macos).
        .with_fullsize_content_view(true)
        .with_titlebar_shown(false)
        .with_title_shown(false);

    // What the app is built from, taken by the renderer that gets the
    // window up.
    let start = RefCell::new(Some((dirs, launch)));
    let create =
        |cc: &eframe::CreationContext<'_>| -> Result<Box<dyn eframe::App>, Box<dyn std::error::Error + Send + Sync>> {
            let Some((dirs, launch)) = start.take() else {
                return Err("Violet has already started".into());
            };
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
            let project = dirs.project(settings.project());
            let repository: Arc<dyn ProfileRepository> =
                Arc::new(JsonProfileRepository::new(&project.profiles_file));
            let (profiles, error) = ProfileService::load(repository, Arc::clone(&secrets));
            if let Some(error) = launch.error {
                startup_errors.push(ui::toast_error(
                    "The update was undone",
                    format!("Violet went back to the version it was. {error}"),
                ));
            }
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
                session: JsonFile::new(project.session_file),
                scripts: ScriptStore::new(project.scripts_dir),
                dirs: dirs.clone(),
                updates: settings
                    .updates
                    .then(|| {
                        let repaint = ctx.clone();
                        update::Updates::start(move || repaint.request_repaint())
                    })
                    .flatten(),
                update_receipt: launch.receipt,
            };
            Ok(Box::new(VioletApp::new(
                &ctx,
                services,
                settings,
                startup_errors,
            )))
        };

    let mut result = Ok(());
    for &renderer in RENDERERS {
        let options = eframe::NativeOptions {
            viewport: viewport.clone(),
            centered: true,
            renderer,
            ..Default::default()
        };
        result = eframe::run_native("Violet", options, Box::new(&create));
        // Only a renderer that could not open the window is worth another
        // try: once the app has started, its error is the app's.
        match &result {
            Err(error) if start.borrow().is_some() => {
                log::warn!("the {renderer} renderer could not start: {error}");
            }
            _ => break,
        }
    }
    // The update helper waits for this process to exit before installing,
    // and gives up after a minute: exit now rather than wait on database
    // sessions and worker threads winding down.
    if update::handed_off() {
        log::logger().flush();
        std::process::exit(0);
    }
    result
}

/// The renderers to open the window with, in order.
///
/// On Windows, OpenGL (glow) first: it takes about a third of the memory
/// wgpu does (~70 MB against ~250 MB), which sets up a Direct3D 12 or
/// Vulkan device with its shader compilers. wgpu stays for machines with
/// no usable OpenGL, such as some virtual machines and remote desktops.
#[cfg(windows)]
const RENDERERS: &[eframe::Renderer] = &[eframe::Renderer::Glow, eframe::Renderer::Wgpu];
#[cfg(not(windows))]
const RENDERERS: &[eframe::Renderer] = &[eframe::Renderer::Wgpu];

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
