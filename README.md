# Violet

A modern, fast database client in the spirit of DBeaver, built with
[egui](https://github.com/emilk/egui) and [fastframe](https://fastframe.dev).
Databases come as **providers**: WebAssembly components Violet downloads
the first time a connection needs one, and runs in a sandbox. PostgreSQL is
the first; anyone can write another with the [SDK](crates/violet-sdk).

## Architecture

```
violet (bin)            composition root + egui interface (src/ui)
  │
  ├── violet-app        application layer: Backend (async runtime, sessions,
  │                     commands → events, providers installed on demand),
  │                     ProfileService, DriverRegistry
  │
  ├── violet-core       domain + ports. No I/O, no UI.
  │                     Driver / Session / Canceller / Dialect traits,
  │                     ProfileRepository / SecretStore / ResultCache /
  │                     ProviderStore traits, ConnectionProfile, ResultSet,
  │                     DbError, Secret
  │
  ├── violet-providers  adapter: the provider catalogue, download, SHA-256
  │                     verification and the providers folder (ProviderStore)
  ├── violet-wasm       adapter: runs provider components on wasmtime, in a
  │                     sandbox, as Drivers
  └── violet-storage    adapters: JSON profiles on disk, keychain secrets,
                        in-memory key-value result cache

violet-sdk              what providers are built with: wit/provider.wit,
                        Provider / Session traits, sockets, rows
violet-provider-cli     `violet-provider`: inspect, install, package
providers/postgres      the PostgreSQL provider (wasm32-wasip2)
```

Dependencies point inward: adapters depend on `violet-core`; `violet-app`
depends only on `violet-core`; only `src/main.rs` names the concrete
adapters. The interface never blocks on a database: it calls
`Backend` methods that return at once and drains `Event`s each frame.

Inside the UI, views only read state and push `Action`s; `VioletApp::apply`
is the single place that mutates state or talks to the backend.

## Providers

DBeaver bundles every driver it might need. Violet starts with none:

1. The connection form lists the databases in the provider catalogue
   (`providers.json`, by default from the latest GitHub release;
   `VIOLET_PROVIDER_INDEX` points elsewhere, a URL or a local file).
2. The first time a connection needs one, Violet downloads its component,
   checks its SHA-256 against the catalogue, compiles it, asks it to
   describe itself, and installs it into the `providers` folder (on macOS
   `~/Library/Application Support/dev.violet.Violet/providers`). Later runs
   use it straight away, offline included.
3. The package button in the title bar lists providers, installs updates
   and removes them.

Each provider runs in its own sandbox, per connection: no files, no
environment, no network except a socket to the host and port of the
profile it serves, with TLS done by Violet. Even a hostile provider cannot
send a password or rows anywhere else.

### Adding a database

Write a provider with [`violet-sdk`](crates/violet-sdk) (the guide walks
through it), build it for `wasm32-wasip2`, and add it to the catalogue with
`violet-provider package`. Violet itself does not change: the connection
form, tree, editor and grid work as they are.

## Browsing data and the result cache

Clicking a table or view in the tree opens its data tab, as in DBeaver
(the chevron still lists its columns). Rows arrive 200 at a time; the
footer fetches the next page, fetches everything up to the row cap, or
counts the rows. Type a `WHERE` condition in the filter bar and press
Enter; click a column header to sort by it (ascending, descending, off).
Sorting and filtering run on the server. A single `SELECT` run from an
editor is paged the same way.

Read-only results are kept in a key-value cache (`ResultCache`, an LRU of
64 MiB in memory) and shown again without a round trip when nothing
changed:

- Only plain reads are cached: `SELECT`/`WITH`/`VALUES`/`TABLE`/`SHOW`/
  `EXPLAIN` without data-modifying CTEs, `SELECT INTO`, `FOR UPDATE`,
  `EXPLAIN ANALYZE`, or functions and views whose answer moves on its own
  (`now()`, `random()`, `nextval()`, `pg_stat_*`, ...).
- Before a cached result is reused, the driver's change token is compared.
  For PostgreSQL it combines every table's write counters with the
  catalog's transaction ids, so writes, DDL and `TRUNCATE` from any session
  invalidate it. Write counters reach other sessions about a second after
  a transaction ends.
- Anything else run through Violet on that connection, a reconnect, or the
  Refresh button drops or bypasses the cache; entries also expire after
  five minutes. Results served from the cache are marked "Cached" with
  their age.

## Security

- Passwords live in the OS keychain (Keychain / Credential Manager /
  Secret Service), never in `connections.json`. Unticking "save password"
  deletes it.
- `Secret` redacts itself in `Debug`/`Display` and is zeroed on drop; form
  buffers are zeroed when dialogs close.
- Profiles are written atomically with mode `0600`.
- Providers are sandboxed (see [Providers](#providers)): each connection
  runs in its own WebAssembly instance, capped at 512 MiB, that can only
  open a socket to that profile's server. Downloads must match the
  catalogue's SHA-256; installed components are checked again on every
  load, so a file changed on disk is refused. Catalogues are fetched over
  HTTPS (plain HTTP only from this machine) and redirects cannot leave
  HTTPS.
- TLS (`Prefer`/`Require`) is done by Violet, not the provider, and verifies
  the server against the web PKI roots. With `Require`, Violet sends at most
  a 64-byte preamble in plain text, whatever the provider tries.
  `Prefer` only falls back to plain text on loopback hosts and Unix sockets;
  for any other host it is enforced as `Require`, so a network attacker
  cannot downgrade the connection and read the password.
- Catalog queries use bind parameters; generated SQL quotes identifiers.
- Result sets are capped (10 000 rows) so a stray `SELECT *` cannot exhaust
  memory; running queries can be cancelled server-side.
- Cached results stay in memory only (never on disk) and go when the app
  closes or the connection does.
- The data filter must be one expression: a `;` or unbalanced bracket is
  refused before anything is sent.
- `unsafe_code = "forbid"` across the workspace (the SDK excepted: the
  component bindings it generates are `unsafe` by nature).

## Usage

```sh
cargo run --release
```

To publish providers, run `scripts/package-providers.sh` and attach the
files in `dist/` to a GitHub release.

| Shortcut | Action |
| --- | --- |
| ⌘/Ctrl + Enter | Run selection, or the statement under the cursor |
| ⇧ + ⌘/Ctrl + Enter | Run the whole script |
| ⌘/Ctrl + . | Cancel the running query |
| ⌘/Ctrl + T / W | New / close editor tab |
| ⌘/Ctrl + N | New connection |
| ⌘/Ctrl + B | Toggle sidebar |

`VIOLET_HOME=<dir>` keeps config and logs under one directory.

## Development

```sh
rustup target add wasm32-wasip2
# Build the providers and a local catalogue, and run against it:
scripts/package-providers.sh dist
VIOLET_PROVIDER_INDEX=$PWD/dist/providers.json cargo run

cargo test --workspace
# The PostgreSQL provider against a real server:
docker run -d --rm -e POSTGRES_PASSWORD=violet -e POSTGRES_USER=violet -p 55499:5432 postgres:17-alpine
VIOLET_TEST_PG=localhost:55499:violet:violet:violet cargo test -p violet-wasm
# Debug builds can capture the window (see src/ui/devtools.rs):
VIOLET_HOME=/tmp/v VIOLET_SNAPSHOT=/tmp/shot.png VIOLET_SCENE=welcome cargo run
```

The egui and winit forks fastframe needs are patched in the root
`Cargo.toml` (`[patch.crates-io]`).
