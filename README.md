# Violet

A modern, fast database client in the spirit of DBeaver, built with
[egui](https://github.com/emilk/egui) and [fastframe](https://fastframe.dev).
PostgreSQL is the first driver; the architecture is built so that each new
database is one new crate.

## Architecture

```
violet (bin)            composition root + egui interface (src/ui)
  │
  ├── violet-app        application layer: Backend (async runtime, sessions,
  │                     commands → events), ProfileService, DriverRegistry
  │
  ├── violet-core       domain + ports. No I/O, no UI.
  │                     Driver / Session / Canceller / Dialect traits,
  │                     ProfileRepository / SecretStore traits,
  │                     ConnectionProfile, ResultSet, DbError, Secret
  │
  ├── violet-postgres   adapter: tokio-postgres + rustls (implements Driver)
  └── violet-storage    adapters: JSON profiles on disk, keychain secrets
```

Dependencies point inward: drivers and storage depend on `violet-core`;
`violet-app` depends only on `violet-core`; only `src/main.rs` names the
concrete adapters. The interface never blocks on a database: it calls
`Backend` methods that return at once and drains `Event`s each frame.

Inside the UI, views only read state and push `Action`s; `VioletApp::apply`
is the single place that mutates state or talks to the backend.

### Adding a database

1. `cargo new --lib crates/violet-<db>` and depend on `violet-core`.
2. Implement `Driver` (connect) and `Session` (catalog, execute, cancel).
3. Register it in `src/main.rs`: `DriverRegistry::new().with(PostgresDriver).with(MyDriver)`.

The connection form, tree, editor and grid work unchanged.

## Browsing data

Clicking a table or view in the tree opens its data tab, as in DBeaver
(the chevron still lists its columns). Rows arrive 200 at a time; the
footer fetches the next page, fetches everything up to the row cap, or
counts the rows. Type a `WHERE` condition in the filter bar and press
Enter; click a column header to sort by it (ascending, descending, off).
Sorting and filtering run on the server. A single `SELECT` run from an
editor is paged the same way.

## Security

- Passwords live in the OS keychain (Keychain / Credential Manager /
  Secret Service), never in `connections.json`. Unticking "save password"
  deletes it.
- `Secret` redacts itself in `Debug`/`Display` and is zeroed on drop; form
  buffers are zeroed when dialogs close.
- Profiles are written atomically with mode `0600`.
- TLS (`Prefer`/`Require`) verifies the server against the web PKI roots.
  `Prefer` only falls back to plain text on loopback hosts and Unix sockets;
  for any other host it is enforced as `Require`, so a network attacker
  cannot downgrade the connection and read the password.
- Catalog queries use bind parameters; generated SQL quotes identifiers.
- Result sets are capped (10 000 rows) so a stray `SELECT *` cannot exhaust
  memory; running queries can be cancelled server-side.
- The data filter must be one expression: a `;` or unbalanced bracket is
  refused before anything is sent.
- `unsafe_code = "forbid"` across the workspace.

## Usage

```sh
cargo run --release
```

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
cargo test --workspace
# Driver tests against a real server:
docker run -d --rm -e POSTGRES_PASSWORD=violet -e POSTGRES_USER=violet -p 55499:5432 postgres:17-alpine
VIOLET_TEST_PG=localhost:55499:violet:violet:violet cargo test -p violet-postgres
# Debug builds can capture the window (see src/ui/devtools.rs):
VIOLET_HOME=/tmp/v VIOLET_SNAPSHOT=/tmp/shot.png VIOLET_SCENE=welcome cargo run
```

The egui and winit forks fastframe needs are patched in the root
`Cargo.toml` (`[patch.crates-io]`).
