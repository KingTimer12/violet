# violet-sdk

Build database providers for [Violet](../../README.md).

Violet ships no database drivers. Each database is a **provider**: a
WebAssembly component that Violet downloads into its `providers` folder the
first time a connection needs it. Providers run in a sandbox and can reach
only one thing: the server of the connection they serve.

```
 Violet                                   provider.wasm (your code)
 ──────                                   ─────────────────────────
 connection form ─ connect(params) ─────▶ Provider::connect
 tree, grid      ─ schemas/objects/... ─▶ Session methods
                 ◀── network.connect ──── net::Connection::open()
 TCP + TLS (rustls, web PKI roots)        protocol, auth, parsing
```

## What a provider can and cannot do

| | |
| --- | --- |
| Network | One kind of socket, to the profile's host and port only (`net::Connection::open`), or a socket file inside the profile's directory (`open_local`). TLS is done by Violet: call `start_tls()` and the certificate is checked against the host the user typed. When the profile requires TLS, Violet refuses to send more than a 64-byte plain-text preamble, so a password cannot leak by mistake. |
| Files, environment, arguments | None. WASI's own sockets are disabled too. |
| Memory | 512 MiB per session. |
| CPU | Runaway code yields to Violet's runtime and is stopped when its session closes. |
| Output | Everything returned is validated by Violet; malformed rows are refused. |

Each session lives in its own instance, so calls never overlap and one
session's crash does not touch another.

## A provider in five steps

**1. Create the crate**, outside Violet's workspace:

```toml
[package]
name = "violet-provider-lite"
version = "0.1.0"
edition = "2024"

[workspace]

[lib]
crate-type = ["cdylib"]

[dependencies]
violet-sdk = { path = "../../crates/violet-sdk" }
log = "0.4"

[profile.release]
opt-level = "s"
lto = true
strip = true
panic = "abort"
```

**2. Describe it** and open sessions:

```rust
use violet_sdk::net::Connection;
use violet_sdk::prelude::*;

pub struct Lite;

impl Provider for Lite {
    type Session = LiteSession;

    fn info() -> ProviderInfo {
        ProviderInfo {
            description: "LiteDB 3 and later".into(),
            default_user: "admin".into(),
            // ANSI quotes, LIMIT/OFFSET paging, schemas, TLS by default.
            ..ProviderInfo::new("lite", "LiteDB", 7000)
        }
    }

    fn connect(params: ConnectParams) -> Result<LiteSession> {
        let mut socket = Connection::open()?;
        if params.ssl != SslMode::Disable {
            // ... ask the server to switch, then:
            socket.start_tls()?;
        }
        // ... handshake, authenticate with params.password ...
        Ok(LiteSession { socket: RefCell::new(socket) })
    }
}

violet_sdk::export_provider!(Lite);
```

`params.ssl` is already the mode to honour: Violet turns `Prefer` into
`Require` for any host that is not this machine.

**3. Implement `Session`**: `server_info`, `schemas`, `objects`, `columns`
and `execute`; `databases`, `change_token` and `cancel_ticket` are
optional. Results go through `RowsBuilder`, which keeps at most `max_rows`
rows and marks the set truncated:

```rust
fn execute(&self, sql: &str, max_rows: usize) -> Result<Vec<ResultSet>> {
    let mut socket = self.socket.borrow_mut();
    // ... send `sql`, read the column list ...
    let columns = vec![ColumnMeta::new("id", Some("int".into()), true)];
    let mut rows = RowsBuilder::new(columns.len(), max_rows);
    // for each row the server sends, even past the cap:
    rows.push([Some("1")])?;
    Ok(vec![ResultSet::query(columns, rows)])
}
```

Errors are `DbError`: `DbError::query(message).with_code("42703")`, with
`position` set (1-based, in characters) to underline the statement.
`Authentication` errors make Violet ask for the password again.

- **Cached results.** Return a value from `change_token` that changes
  whenever data or schema may have (PostgreSQL sums its write counters):
  Violet then reuses unchanged `SELECT` results.
- **Cancel.** Return the data needed to stop a statement from
  `cancel_ticket` (PostgreSQL's backend key), and implement
  `Provider::cancel`; it runs in a fresh instance while the session is busy.
- **Logs.** `log::info!` and friends land in Violet's log, tagged with the
  provider id.

**4. Build** it as a component:

```sh
rustup target add wasm32-wasip2
cargo build --release --target wasm32-wasip2
```

**5. Try it, then publish it** with the `violet-provider` tool
(`cargo run -p violet-provider-cli -- ...` in Violet's repository):

```sh
violet-provider inspect target/wasm32-wasip2/release/violet_provider_lite.wasm
violet-provider install target/wasm32-wasip2/release/violet_provider_lite.wasm
violet-provider package --out dist target/wasm32-wasip2/release/*.wasm
```

`inspect` checks the component against the provider API and prints what it
declares. `install` puts it straight into Violet's providers folder.
`package` writes `dist/<id>-<version>.wasm` and adds it, with its SHA-256,
to `dist/providers.json`. Publish both files over HTTPS (Violet reads the
latest GitHub release by default), or point Violet at the catalogue:

```sh
VIOLET_PROVIDER_INDEX=$PWD/dist/providers.json cargo run
```

## The contract

The interface is [`wit/provider.wit`](wit/provider.wit), a WebAssembly
component interface: providers can also be written in any language with
component tooling (C, Go, JavaScript...), as long as they target the
`violet:provider/provider-world` world. The API version
(`violet_sdk::API_VERSION`) follows semver; Violet refuses providers built
for an incompatible one.

See [`providers/postgres`](../../providers/postgres) for a complete
provider: the PostgreSQL wire protocol, SCRAM-SHA-256 authentication,
TLS negotiation, catalog queries with bind parameters and cancellation.
