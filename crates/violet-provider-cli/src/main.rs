//! `violet-provider`: tooling for provider authors.
//!
//! ```text
//! violet-provider inspect <provider.wasm>
//!     Compiles a provider, checks it against the provider API and prints
//!     what it says about itself.
//!
//! violet-provider package --out <dir> <provider.wasm>...
//!     Copies providers into <dir> as <id>-<version>.wasm and adds them to
//!     <dir>/providers.json, the catalogue Violet installs from. Publish the
//!     folder (a GitHub release, any HTTPS host) and point Violet at it with
//!     VIOLET_PROVIDER_INDEX, or at the folder itself while developing.
//!
//! violet-provider install <provider.wasm>
//!     Installs a provider straight into Violet's providers folder
//!     (VIOLET_HOME is honoured), skipping the catalogue.
//! ```

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use violet_providers::{Entry, Index, Providers, Source, sha256};
use violet_storage::AppDirs;
use violet_wasm::{Description, Runtime};

const USAGE: &str = "usage:
  violet-provider inspect <provider.wasm>
  violet-provider package --out <dir> <provider.wasm>...
  violet-provider install <provider.wasm>";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => return fail(&error.to_string()),
    };
    let result = runtime.block_on(async {
        match args.first().map(String::as_str) {
            Some("inspect") => match &args[1..] {
                [file] => inspect(Path::new(file)).await,
                _ => Err(USAGE.to_owned()),
            },
            Some("package") => match &args[1..] {
                [flag, out, files @ ..] if flag == "--out" && !files.is_empty() => {
                    package(Path::new(out), files).await
                }
                _ => Err(USAGE.to_owned()),
            },
            Some("install") => match &args[1..] {
                [file] => install(Path::new(file)).await,
                _ => Err(USAGE.to_owned()),
            },
            _ => Err(USAGE.to_owned()),
        }
    });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => fail(&message),
    }
}

fn fail(message: &str) -> ExitCode {
    eprintln!("{message}");
    ExitCode::FAILURE
}

async fn describe(file: &Path) -> Result<(Vec<u8>, Description), String> {
    let wasm = std::fs::read(file).map_err(|e| format!("{}: {e}", file.display()))?;
    let runtime = Runtime::new(None).map_err(|e| e.to_string())?;
    let description = runtime
        .inspect(wasm.clone())
        .await
        .map_err(|e| format!("{}: {e}", file.display()))?;
    Ok((wasm, description))
}

async fn inspect(file: &Path) -> Result<(), String> {
    let (wasm, description) = describe(file).await?;
    let info = &description.info;
    println!("{} {} ({})", info.name, description.version, info.id);
    if !description.description.is_empty() {
        println!("  {}", description.description);
    }
    println!("  provider API  {}", violet_wasm::API_VERSION);
    println!("  default port  {}", info.default_port);
    println!(
        "  defaults      database {:?}, user {:?}",
        info.default_database, info.default_user
    );
    println!("  capabilities  {:?}", info.capabilities);
    println!("  dialect       {:?}", info.dialect);
    println!("  size          {} bytes", wasm.len());
    println!("  sha256        {}", sha256(&wasm));
    Ok(())
}

async fn package(out: &Path, files: &[String]) -> Result<(), String> {
    std::fs::create_dir_all(out).map_err(|e| format!("{}: {e}", out.display()))?;
    let index_path = out.join("providers.json");
    let mut index = match std::fs::read(&index_path) {
        Ok(bytes) => Index::parse(&bytes).map_err(|e| e.to_string())?,
        Err(_) => Index::default(),
    };
    for file in files {
        let (wasm, description) = describe(Path::new(file)).await?;
        let id = description.info.id.as_str().to_owned();
        let name = format!("{id}-{}.wasm", description.version);
        let target = out.join(&name);
        std::fs::write(&target, &wasm).map_err(|e| format!("{}: {e}", target.display()))?;
        println!("{name}  {}", sha256(&wasm));
        index.upsert(Entry {
            id,
            version: description.version,
            api: violet_wasm::api_level(&description.info).to_owned(),
            description: description.description,
            url: name,
            sha256: sha256(&wasm),
            size: wasm.len() as u64,
            info: description.info,
        });
    }
    let json = serde_json::to_vec_pretty(&index).map_err(|e| e.to_string())?;
    std::fs::write(&index_path, json).map_err(|e| format!("{}: {e}", index_path.display()))?;
    println!("wrote {}", index_path.display());
    Ok(())
}

async fn install(file: &Path) -> Result<(), String> {
    let wasm = std::fs::read(file).map_err(|e| format!("{}: {e}", file.display()))?;
    let dirs = std::env::var_os("VIOLET_HOME")
        .map(AppDirs::at)
        .or_else(AppDirs::discover)
        .ok_or("Could not find Violet's folders; set VIOLET_HOME")?;
    let runtime = Runtime::new(None).map_err(|e| e.to_string())?;
    let providers = Providers::new(
        dirs.providers_dir(),
        runtime,
        Source::File(PathBuf::from("/dev/null")),
    );
    let driver = providers
        .install_component(wasm, None)
        .await
        .map_err(|e| e.to_string())?;
    println!(
        "installed {} into {}",
        driver.info().name,
        dirs.providers_dir().display()
    );
    Ok(())
}
