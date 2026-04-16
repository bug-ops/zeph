# zeph-db

Database abstraction layer for [Zeph](https://github.com/bug-ops/zeph) — unified SQLite and PostgreSQL backends with compile-time backend selection, automatic migrations, dialect-aware SQL helpers, and FTS support.

**Important:**
> Exactly one of the `sqlite` or `postgres` features must be enabled. The default is `sqlite`. Enabling both simultaneously triggers a `compile_error!`. Using `--all-features` is intentionally unsupported — use `--features full` or `--features full,postgres` instead.

## Features

- **Compile-time backend selection** — `DbPool`, `DbRow`, `DbTransaction`, and `DbQueryResult` resolve to the correct sqlx types based on the active feature
- **`sql!` macro** — write `?` placeholders once; the macro rewrites them to `$1, $2, ...` for PostgreSQL and is a zero-cost no-op for SQLite
- **`Dialect` trait** — backend-specific SQL constants (`AUTO_PK`, `INSERT_IGNORE`, `EPOCH_NOW`, etc.) and helpers (`ilike`, `epoch_from_col`) via zero-sized marker types
- **Automatic migrations** — `DbConfig::connect` runs `migrations/sqlite/` or `migrations/postgres/` on startup; WAL checkpoint applied after SQLite migrations
- **`FullDriver` super-trait** — reduces sqlx bound repetition in generic impl blocks across consumer crates
- **FTS helpers** — backend-aware `WHERE`/`JOIN`/rank fragments for messages and graph entity full-text search
- **Safe URL logging** — `redact_url` strips credentials from connection strings before they appear in logs
- **Write transactions** — `begin_write` issues `BEGIN IMMEDIATE` on SQLite (prevents `SQLITE_BUSY`); falls back to standard `BEGIN` on PostgreSQL

## Runtime backend selection

The active backend is determined at compile time by feature flag. For deployments that need to switch between SQLite and PostgreSQL without recompiling, set `ZEPH_DATABASE_URL` or `database_url` in `config.toml`:

```toml
[database]
database_url = "postgres://user:pass@localhost/zeph"
```

```bash
ZEPH_DATABASE_URL=postgres://user:pass@localhost/zeph zeph
```

**Important:**
> The URL scheme (`sqlite:` / `postgres:`) must match the compiled feature. A `postgres://` URL with the `sqlite` feature (or vice versa) will fail at startup with a clear error.

## CLI migrations

Run pending migrations without starting the agent:

```bash
zeph db migrate                               # apply pending migrations using config database_url
zeph db migrate --url postgres://user:pass@localhost/zeph
```

**Tip:**
> Use `zeph db migrate --dry-run` to print the SQL that would be applied without executing it.

## Installation

This crate is an internal workspace member of Zeph. To use it in a workspace crate:

```toml
[dependencies]
zeph-db = { path = "../zeph-db" }
# or with postgres backend:
zeph-db = { path = "../zeph-db", default-features = false, features = ["postgres"] }
```

## Feature Flags

| Feature | Description |
|---------|-------------|
| `sqlite` (default) | Enables SQLite backend via `sqlx/sqlite` |
| `postgres` | Enables PostgreSQL backend via `sqlx/postgres` |
| `test-utils` | Enables `testcontainers` + `testcontainers-modules` for PostgreSQL integration tests; implies `postgres` |

## Usage

### Connect and run migrations

```rust
use zeph_db::{DbConfig, DbPool};

let config = DbConfig {
    url: "path/to/zeph.db".into(),
    max_connections: 5,
    pool_size: 5,
};

let pool: DbPool = config.connect().await?;
```

For in-memory SQLite (useful in tests):

```rust
let pool = DbConfig { url: ":memory:".into(), ..Default::default() }
    .connect()
    .await?;
```

### Write portable SQL with the `sql!` macro

```rust
use zeph_db::sql;

let rows = sqlx::query(sql!("SELECT id FROM messages WHERE conversation_id = ?"))
    .bind(conversation_id)
    .fetch_all(&pool)
    .await?;
```

**Note:**
> Do not use the `sql!` macro for PostgreSQL JSONB queries that contain `?`, `?|`, or `?&` operators — use `$N` placeholders directly for those.

### Dialect-aware SQL fragments

```rust
use zeph_db::{ActiveDialect, Dialect};

let ddl = format!("CREATE TABLE t (id {}, name TEXT)", ActiveDialect::AUTO_PK);
let insert = format!("{} INTO t (name) VALUES (?){}", ActiveDialect::INSERT_IGNORE, ActiveDialect::CONFLICT_NOTHING);
```

### Transactions

```rust
use zeph_db::{begin, begin_write};

// Standard deferred transaction
let mut tx = begin(&pool).await?;

// Write-intent transaction (BEGIN IMMEDIATE on SQLite)
let mut tx = begin_write(&pool).await?;
sqlx::query("INSERT INTO t (name) VALUES (?)").bind("foo").execute(&mut *tx).await?;
tx.commit().await?;
```

### FTS helpers

```rust
use zeph_db::fts::{sanitize_fts_query, messages_fts_where, messages_fts_join, messages_fts_rank_select, messages_fts_order_by};

let q = sanitize_fts_query(user_input);
let sql = format!(
    "SELECT m.id, {} FROM messages m {} WHERE {} ORDER BY {}",
    messages_fts_rank_select(),
    messages_fts_join(),
    messages_fts_where(),
    messages_fts_order_by(),
);
```

### Generic consumer crates

Use `D: DatabaseDriver + FullDriver` as the single generic bound when you need both sqlx pool access and SQL dialect fragments:

```rust
use zeph_db::{DatabaseDriver, FullDriver, DbConfig};

async fn init_store<D: DatabaseDriver + FullDriver>(config: DbConfig) -> sqlx::Pool<D::Database> {
    config.connect().await.expect("db init")
}
```

## Migrations

SQL migration files live in:

- `migrations/sqlite/` — SQLite DDL (FTS5 virtual tables, triggers, indexes)
- `migrations/postgres/` — PostgreSQL DDL (tsvector columns, GIN indexes, `plainto_tsquery` setup)

Migrations run automatically on first `DbConfig::connect` call. The active backend's directory is embedded at compile time via `sqlx::migrate!`.

## MSRV

Rust **1.94** (Edition 2024, resolver 3).

## License

MIT — see [LICENSE](../../LICENSE).
