# Violet

A modern, fast database client in the spirit of DBeaver, built with
[egui](https://github.com/emilk/egui) and [fastframe](https://fastframe.dev).
Databases come as **providers**: WebAssembly components Violet downloads
the first time a connection needs one, and runs in a sandbox. PostgreSQL is
the first; anyone can write another with the [SDK](crates/violet-sdk).

## Compared with DBeaver

| | Violet 0.1.0 | DBeaver 26.0.3 |
| --- | --- | --- |
| Size on disk | 23.7 MB + 0.3 MB PostgreSQL provider | 202 MB |
| Launch to main window | 166 ms | 3070 ms |
| Memory when idle (footprint) | 82 MB | 561 MB |
| CPU when idle | 0.1 % | 2.3 % |

Apple M4, 24 GB, macOS 15.7.3; median of 3 runs, measured on
2026-10-14 with [`scripts/bench-dbeaver.sh`](scripts/bench-dbeaver.sh).
Both are measured with no connection open, 15 s after the main window
shows; DBeaver with its own bundled Java runtime and an existing
workspace, Violet with an empty one. Memory is macOS's footprint, what
Activity Monitor shows as "Memory". Run the script to compare on your
machine.

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

## Code completion

The editor completes as you type a name or a dot, and on ⌃Space. What it
offers depends on where the cursor is:

- after `FROM`, `JOIN`, `UPDATE`, `INTO`: tables and views (unqualified
  from the search path, `schema.table` from the others), CTEs and schemas;
  in `FROM` and `JOIN` a table comes with an alias, as in DBeaver
  (`seguranca.usuarios u`), and `u.` lists its columns right away;
- after `JOIN`, whole joins along foreign keys first:
  `orders o ON o.customer_id = c.id`; after `ON`, the conditions alone;
- in `SELECT`, `WHERE`, `ORDER BY`...: the columns of the tables the
  statement reads, with their types, qualified by alias when two tables
  share a name, then functions and keywords;
- after `alias.`, `table.` or `schema.`: that table's columns or that
  schema's objects; after `INSERT INTO t (`: `t`'s columns; after `::`,
  types.

It knows the statement's aliases, subqueries and `WITH` queries, and the
tables, views and foreign keys the script itself creates (`CREATE TABLE`
with `REFERENCES`, `CREATE VIEW ... AS`) before they exist on the server.
Columns and foreign keys are read from the server the first time they are
needed; foreign keys come from the standard `information_schema`, so any
database that has it is covered. ↑↓ choose, ↵ or ⇥ insert, esc closes.

## Query console

Each SQL editor runs queries in several modes, from the toolbar or the
keyboard:

- **Run** the selection, or the statement under the cursor; a single
  `SELECT` is read a page at a time.
- **Run script**: every statement in the editor.
- **Explain plan**: the statement under the cursor behind `EXPLAIN`, to see
  how the server would run it without running it.
- **Explain analyze**: the statement run for real and timed by the server.
  Since that executes it, Violet only does it for statements that read and
  call no function known to have side effects (`nextval`, `pg_notify`...).
  Functions you wrote yourself cannot be told apart, so it is a guard
  against accidents, not a sandbox.

Errors are marked in the editor where the server points, explain modes
included.

Nothing is lost between runs:

- **Query history** (⌘/Ctrl + Y, or the clock in the toolbar): every query
  run from an editor, with its connection, time, duration and rows, or the
  error. Search it, keep to the current connection, and put a query back at
  the cursor or in a new editor. It lives in `history.jsonl` in Violet's
  state directory, readable by the user only, trimmed to the last 5,000
  queries; statements that may carry a password (`PASSWORD`,
  `IDENTIFIED BY`, `CONNECTION '...'`, or a literal mentioning a password)
  are kept with their literals replaced by `'•••'` and their error message
  hidden.
- **Open editors** come back as they were: their text, titles and
  connections are saved as you type (`session.json`, next to the history,
  mode `0600`) and reopened on the next start. Like a `.sql` file you
  save, it keeps the text exactly as typed, passwords included.

## Backup and restore

Right-click a connection (**Backup…**, **Restore…**), a schema (**Backup
schema…**) or a table (**Backup table…**). As in DBeaver, Violet runs the
database's own client programs; for PostgreSQL, `pg_dump`, `pg_restore`
and `psql`. Each provider describes its own options, so every database
brings its own dialog.

PostgreSQL backups offer DBeaver's options and more:

- **Objects**: the whole database, or any mix of schemas and tables, picked
  from the tree.
- **Format**: custom, directory (dumped in parallel jobs), tar or plain
  SQL; compression level; encoding.
- **Content**: schema and data, schema only or data only; data as `COPY`,
  `INSERT` or `INSERT` with column names, several rows per `INSERT`, `ON
  CONFLICT DO NOTHING`; large objects; tables to skip, or to keep without
  their rows.
- **Ownership**: discard owners, privileges, tablespaces or comments; `SET
  SESSION AUTHORIZATION`.
- **Script** (plain SQL): drop objects first (with `IF EXISTS`), create the
  database, disable triggers while data loads.
- **Advanced**: lock wait timeout, a detailed log, extra arguments.
- **Output**: a folder and a file name with variables (`${database}`,
  `${connection}`, `${host}`, `${schema}`, `${date}`, `${time}`,
  `${timestamp}`).

Restores read the backup's format from the file itself and run
`pg_restore` (archives) or `psql` (plain SQL), into the connection's
database or another one: drop objects first, create the database, single
transaction, stop at the first error, parallel jobs, schema or data only,
discard owners or privileges, run as a role, disable triggers.

While a task runs, its log streams in, with the time taken and the size
written so far; **Stop** ends it. When it finishes, **Show in folder** opens
the backup, and a restore refreshes the tree. The options, folders and
client programs used last are remembered.

**No client to install.** A provider may name client programs to download
with it, per platform; PostgreSQL's brings `pg_dump`, `pg_restore` and
`psql` 18.6 (about 13 MB, from
[theseus-rs/postgresql-binaries](https://github.com/theseus-rs/postgresql-binaries)),
fetched in the background right after the provider installs. The archive
is checked against the SHA-256 the provider declares and only the
programs and their libraries are kept, under `providers/.tools/`. A folder
you pick comes first; then the downloaded programs, the usual places
(Homebrew, Postgres.app, EnterpriseDB, `/usr/lib/postgresql/*/bin`...)
and `PATH`. Versions show, with a warning when the client is older than
the server.

**What runs, and with what.** Unlike providers, these programs run outside
the sandbox, as you: they read and write files and connect to the server
themselves. So:

- a provider does not build command lines. It declares each command up
  front as a template (fixed text, setting values, the chosen path), and
  Violet fills it in; nothing the provider computes later reaches a
  command line;
- the connection is Violet's: host, port, user, database and password come
  from the profile, through the variables or options the provider names
  for them (`PGHOST`, `PGPASSWORD`...), and the template may not set those
  any other way. Values that a client would read as a whole connection
  string (`host=...`, `postgresql://...`) are refused;
- the first backup or restore with a provider asks whether it may run its
  programs, listing everything it may put on a command line and what it
  downloads;
- templates may only name the programs they declare, never a shell, an
  interpreter or a launcher, nor variables that change what runs (`PATH`,
  `LD_*`, `DYLD_*`, `*FILE`...), and their fixed text may not hold pipes or
  console commands (`\!`, `\o`, `\copy`...);
- the full command shows before it starts, and runs without a shell.

What you type yourself (extra arguments, patterns) goes through as typed.

Plain SQL restores run in `psql`, which also runs meta-commands written in
the file (including `\!`, a shell command): only restore files you trust.

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
  runs in its own WebAssembly instance, capped at 512 MiB and at a budget
  of instructions per call, that can only open a few sockets, all to that
  profile's server. Downloads must match the
  catalogue's SHA-256; installed components are checked again on every
  load, so a file changed on disk is refused. Catalogues are fetched over
  HTTPS (plain HTTP only from this machine) and redirects cannot leave
  HTTPS.
- TLS (`Prefer`/`Require`) is done by Violet, not the provider, and verifies
  the server against the web PKI roots. With `Require`, Violet sends at most
  a 64-byte preamble in plain text per connection, across all the sockets
  its provider opens, whatever the provider tries.
  `Prefer` only falls back to plain text on loopback hosts and Unix sockets;
  for any other host it is enforced as `Require`, so a network attacker
  cannot downgrade the connection and read the password.
- Catalog queries use bind parameters; generated SQL quotes identifiers.
- Result sets are capped (10 000 rows) so a stray `SELECT *` cannot exhaust
  memory; running queries can be cancelled server-side.
- Cached results stay in memory only (never on disk) and go when the app
  closes or the connection does.
- The query history and the saved editors are written with mode `0600`;
  string literals in statements that set passwords never reach the history.
- The data filter must be one expression: a `;` or unbalanced bracket is
  refused before anything is sent.
- `unsafe_code = "forbid"` across the workspace (the SDK excepted: the
  component bindings it generates are `unsafe` by nature).

## Usage

```sh
cargo run --release
```

### Releases

`.github/workflows/release.yml` builds Violet for every platform, and the
providers. Push a tag to publish them as a GitHub release:

```sh
git tag v0.1.0 && git push origin v0.1.0
```

| Platform | File | Install |
| --- | --- | --- |
| Windows x64 | `violet-<tag>-windows-x64.zip` | Unzip and run `violet.exe` |
| macOS 11+ (Apple silicon and Intel) | `violet-<tag>-macos-universal.dmg` | Drag Violet to Applications; the first time, right click it > Open (the app is not notarized) |
| Linux x86_64 / aarch64 | `violet-<tag>-linux-<arch>.tar.gz` | Unpack and run `./install.sh` (to `~/.local`), or `./violet` in place |

Each comes with its SHA-256 (`.sha256`). At the release's top level sit
`providers.json` and each provider's `.wasm`, where Violet's default
catalogue URL finds them. Running the workflow by hand builds the same
files as artifacts without publishing. The packages are made by
`scripts/package-{macos,linux}.sh`, which work the same locally; the app
icons come from `assets/app/violet.svg` (`scripts/app-icons.sh`).

| Shortcut | Action |
| --- | --- |
| ⌘/Ctrl + Enter | Run selection, or the statement under the cursor |
| ⇧ + ⌘/Ctrl + Enter | Run the whole script |
| ⌘/Ctrl + . | Cancel the running query |
| ⌘/Ctrl + E | Explain the statement under the cursor |
| ⇧ + ⌘/Ctrl + E | Explain analyze it (reads only) |
| ⌘/Ctrl + Y | Query history |
| ⌃ + Space | Complete the name under the cursor |
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

### Profiling

Violet is instrumented with [hotpath](https://hotpath.rs): frames and
the grid, queries end to end, every provider call, socket reads and
writes, provider compilation and downloads, the result cache. Without the
feature the macros compile to the plain code.

```sh
# Report on exit: time per function (calls, avg, P95, total).
cargo run --release --features hotpath
# Allocations too:
cargo run --release --features hotpath-alloc
# Live, while Violet runs (another terminal):
cargo install hotpath --features tui --version '^0.28'
hotpath
```

`HOTPATH_OUTPUT_PATH=report.txt` writes the report to a file (release
builds on Windows have no console); `HOTPATH_OUTPUT_FORMAT=json-pretty`
for JSON; `HOTPATH_FOCUS=WasmSession` to show some functions only. The
live data is served on `localhost:6770`.

The egui and winit forks fastframe needs are patched in the root
`Cargo.toml` (`[patch.crates-io]`).
