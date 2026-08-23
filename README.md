# osage

A terminal SQL IDE. The core loop — browse a catalog, write SQL, run it,
read the grid — as a keyboard-driven TUI, talking to every database through
[ADBC](https://arrow.apache.org/adbc/) with Arrow as the only data format in
the pipeline.

```
┌───────────────────────────────────────────────────────┐
│ ● prod-pg   ○ local-duckdb                            │
├─────────────────┬─────────────────────────────────────┤
│ CATALOG         │ EDITOR                              │
│  ▼ postgres     │ SELECT * FROM orders o              │
│    ▼ public     │ JOIN customers c ON c.id = o.cust…  │
│      ▶ orders   ├─────────────────────────────────────┤
│      ▶ customers│ RESULTS                             │
│                 │ id │ name  │ total                  │
├─────────────────┴─────────────────────────────────────┤
│ 142 rows · 38ms · prod-pg                             │
└───────────────────────────────────────────────────────┘
```

## Install

```sh
cargo install osage
```

## Drivers

osage does not bundle database drivers — it loads ADBC drivers at runtime. Point
a profile at a driver by name (resolved through ADBC's driver manifests) or by
an absolute path to the shared library.

Manifests live in `~/Library/Application Support/ADBC/Drivers` on macOS and
`~/.config/adbc/drivers` on Linux:

```toml
# ~/Library/Application Support/ADBC/Drivers/duckdb.toml
manifest_version = 1
name = "DuckDB"

[Driver]
shared = "/usr/local/lib/libduckdb.dylib"
entrypoint = "duckdb_adbc_init"
```

Prebuilt drivers for PostgreSQL, SQLite, Snowflake and others ship as Python
wheels; the shared library inside them works fine on its own:

```sh
uv pip install adbc-driver-postgresql
# then point `shared` at .../site-packages/adbc_driver_postgresql/libadbc_driver_postgresql.so
```

## Profiles

`~/.config/osage/profiles.toml`:

```toml
[[profile]]
id = "prod-pg"
name = "Production Postgres"
driver = "postgresql"
uri = "postgresql://host:5432/db"
username = "readonly"
secret_ref = "osage/prod-pg"
color = "red"

[profile.options]
"adbc.connection.autocommit" = "true"
```

Passwords are never written to this file. They live in the OS keychain, keyed by
`secret_ref`; the profile form writes them there for you.

## Keybindings

| Key | Action |
|---|---|
| `Ctrl-h` / `Ctrl-l` | cycle pane focus left/right |
| `Ctrl-j` / `Ctrl-k` | cycle pane focus down/up |
| `Enter` / `Space` | expand/collapse catalog node (Catalog focused) |
| `Ctrl-i` | insert qualified table name at editor cursor |
| `r` | refresh catalog node under cursor (Catalog focused) |
| `c` | connect/disconnect the selected profile (Catalog focused) |
| `Ctrl-Enter` or `F5` | run the statement under the cursor |
| `Ctrl-t` | new editor tab |
| `Ctrl-w` | close tab |
| `Ctrl-e` | export results |
| `Ctrl-Space` | force autocomplete |
| `Tab` / `Enter` | accept completion (popup open) |
| `Esc` | dismiss popup / cancel modal, or abort a running statement |
| `:` | command palette |
| `Ctrl-q` | quit |

`F5` exists because many terminals cannot send `Ctrl-Enter`.

## Headless mode

`--probe` runs the same connection, catalog and query code the UI uses, without
the UI. It is the quickest way to tell whether a driver and URI work at all:

```sh
osage --probe duckdb /tmp/warehouse.duckdb "SELECT 1"
osage --probe postgresql "postgresql://user@host:5432/db" "SELECT count(*) FROM orders"
```

It prints the catalog as JSON on stdout, then the query result as a table.

## Design notes

- **Arrow end to end.** Result batches go from the ADBC driver to the results
  grid without an intermediate representation. Cells are formatted for display
  only for the rows actually on screen.
- **Nothing blocks the render thread.** Every connection owns a dedicated OS
  thread; the UI talks to it over channels and repaints on replies.
- **The catalog is lazy.** Expanding a node issues one `GetObjects` call scoped
  to that node's depth. The full catalog is never fetched eagerly. Results are
  cached until you refresh with `r`.
- **Completion reads the cache only.** Suggestions never trigger a round trip,
  so typing stays instant on a slow connection.

## License

MIT
