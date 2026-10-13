//! Installing the PostgreSQL provider from a local catalogue. Needs it
//! built (skipped otherwise):
//!
//! ```sh
//! cargo build --release --target wasm32-wasip2 \
//!     --manifest-path providers/postgres/Cargo.toml
//! ```

use std::path::{Path, PathBuf};

use violet_core::ports::ProviderStore;
use violet_core::{ConnectionProfile, Credentials, DriverId, ErrorKind};
use violet_providers::{Entry, Index, Providers, Source, sha256};
use violet_wasm::Runtime;

fn component() -> Option<Vec<u8>> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(
        "../../providers/postgres/target/wasm32-wasip2/release/violet_provider_postgres.wasm",
    );
    std::fs::read(path).ok()
}

/// A scratch directory holding a catalogue with `wasm` as `postgres`.
fn catalogue(wasm: &[u8], digest: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("violet-install-{}", unique()));
    let dist = root.join("dist");
    std::fs::create_dir_all(&dist).expect("dist");
    std::fs::write(dist.join("postgres.wasm"), wasm).expect("wasm");
    let mut index = Index::default();
    index.upsert(Entry {
        id: "postgres".into(),
        version: "0.1.0".into(),
        api: violet_wasm::API_VERSION.into(),
        description: String::new(),
        url: "postgres.wasm".into(),
        sha256: digest.into(),
        size: wasm.len() as u64,
        info: serde_json::from_value(serde_json::json!({
            "id": "postgres", "name": "PostgreSQL", "default_port": 5432,
            "default_database": "postgres", "default_user": "postgres",
            "capabilities": {"databases": true, "schemas": true, "cancel": true, "tls": true},
            "dialect": {"quote_open": "\"", "quote_close": "\"", "paging": "limit-offset"}
        }))
        .expect("info"),
    });
    std::fs::write(
        dist.join("providers.json"),
        serde_json::to_vec(&index).expect("json"),
    )
    .expect("index");
    root
}

fn unique() -> String {
    format!(
        "{}-{:?}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    )
}

fn store(root: &Path) -> Providers {
    Providers::new(
        root.join("providers"),
        Runtime::new(None).expect("runtime"),
        Source::File(root.join("dist/providers.json")),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn installs_verifies_and_refuses_tampering() {
    let Some(wasm) = component() else {
        eprintln!("provider not built; skipping");
        return;
    };
    let root = catalogue(&wasm, &sha256(&wasm));
    let providers = store(&root);
    let id = DriverId::new("postgres");

    let listed = providers.available().await.expect("available");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].installed, None);

    let driver = providers.install(&id).await.expect("install");
    assert_eq!(driver.info().name, "PostgreSQL");
    assert_eq!(providers.installed().expect("installed").len(), 1);
    let listed = providers.available().await.expect("available");
    assert_eq!(listed[0].installed.as_deref(), Some("0.1.0"));

    // A fresh start finds it; a changed file on disk is refused on load.
    let installed = store(&root).installed().expect("installed");
    std::fs::write(root.join("providers/postgres/provider.wasm"), b"evil").expect("tamper");
    let mut profile = ConnectionProfile::new(id.clone(), 1);
    profile.username = "x".into();
    let error = installed[0]
        .connect(&profile, &Credentials::default())
        .await
        .err()
        .expect("refused");
    assert_eq!(error.kind, ErrorKind::Provider);
    assert!(
        error.message.contains("changed since it was installed"),
        "{error}"
    );

    providers.remove(&id).await.expect("remove");
    assert!(providers.installed().expect("installed").is_empty());
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_download_that_does_not_match_its_digest_is_not_installed() {
    let Some(wasm) = component() else {
        return;
    };
    let root = catalogue(&wasm, &sha256(b"something else"));
    let providers = store(&root);
    let error = providers
        .install(&DriverId::new("postgres"))
        .await
        .err()
        .expect("refused");
    assert!(error.message.contains("digest"), "{error}");
    assert!(providers.installed().expect("installed").is_empty());
    let _ = std::fs::remove_dir_all(root);
}
