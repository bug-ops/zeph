# Database Abstraction Layer: Multi-Backend Support (SQLite + PostgreSQL)

> **Status**: Phase 1-3 Implemented; Phase 4+ planned
> **Date**: 2026-03-28 (updated 2026-03-29)
> **Scope**: Cross-cutting (zeph-memory, zeph-scheduler, zeph-mcp, zeph-orchestration, zeph-index, zeph-core)

## 1. Problem Statement

Zeph uses SQLite exclusively for all persistence: conversation history, memory,
graph, skills, scheduler, MCP trust scores, plan cache, code index metadata, and
embeddings. SQLite is ideal for single-user desktop deployments but becomes a
bottleneck for:

- **Multi-instance server deployments** (gateway, A2A) where concurrent writes
  from multiple processes deadlock on SQLite's single-writer lock.
- **Cloud/team deployments** where a shared PostgreSQL database is the standard
  infrastructure pattern.
- **Large-scale memory** where PostgreSQL's native JSONB, GIN indexes, and
  `pg_trgm` outperform SQLite's TEXT-based JSON storage and FTS5 for
  full-text/structured queries.

### Goal

Introduce a database abstraction layer that allows Zeph to run against either
SQLite (default, zero-config) or PostgreSQL (opt-in, server deployments) with a
single `backend = "sqlite" | "postgres"` config toggle and no code duplication in
business logic.

### Out of Scope

- MySQL/MariaDB support.
- Multi-database routing (read replicas, sharding).
- Online migration between backends (export/import tooling is future work).
- Changing the Qdrant integration (vector store remains separate).

---

## 2. Current State Analysis

### 2.1 SQLite Usage Inventory

| Crate | Store Type | Pool Source | Schema Strategy | SQLite-Specific Features |
|-------|-----------|-------------|-----------------|--------------------------|
| `zeph-memory` | `SqliteStore` | `SqlitePool` owned | `sqlx::migrate!("./migrations")` — 49 migration files | FTS5 virtual tables, `datetime('now')`, `AUTOINCREMENT`, `COLLATE NOCASE`, `PRAGMA wal_checkpoint`, `PRAGMA busy_timeout`, `PRAGMA journal_mode`, `BEGIN IMMEDIATE`, `INSERT OR IGNORE`, `INSERT OR REPLACE`, `RETURNING`, BLOB for vectors |
| `zeph-memory` | `GraphStore` | `SqlitePool` clone | Shared migrations via `SqliteStore` | FTS5 (`graph_entities_fts`), `COLLATE NOCASE`, `datetime('now')`, `RETURNING` |
| `zeph-memory` | `ResponseCache` | `SqlitePool` clone | Shared migrations | Unix timestamps as `i64` |
| `zeph-memory` | `SqliteVectorStore` | `SqlitePool` clone | Shared migrations | BLOB storage for vectors, in-memory cosine similarity |
| `zeph-memory` | `EmbeddingStore` | `SqlitePool` clone | Shared migrations | Metadata queries against messages table |
| `zeph-scheduler` | `JobStore` | `SqlitePool` owned | Inline `CREATE TABLE IF NOT EXISTS` | `AUTOINCREMENT`, `datetime('now')`, `ALTER TABLE ADD COLUMN` for schema upgrades |
| `zeph-mcp` | `TrustScoreStore` | `SqlitePool` clone | Inline `CREATE TABLE IF NOT EXISTS` | `INTEGER PRIMARY KEY` (no AUTOINCREMENT) |
| `zeph-orchestration` | `PlanCache` | `SqlitePool` clone | Shared migrations (plan_cache table in zeph-memory migrations) | BLOB for embeddings, `INSERT OR REPLACE` |
| `zeph-index` | `CodeStore` | `SqlitePool` clone | Metadata in SQLite, vectors in Qdrant | Metadata only (file paths, hashes) |
| `zeph-core` | Agent persistence | `SqlitePool` clone | Uses `SqliteStore` methods | No direct SQL |

### 2.2 Query Patterns

All queries use the **runtime** `sqlx::query()` / `sqlx::query_as()` / `sqlx::query_scalar()`
builder API. **Zero compile-time query macros** (`query!`, `query_as!`) are used anywhere in
the codebase. This is the single most important factor enabling abstraction.

Transaction patterns:
- `pool.begin().await` (standard deferred transactions) -- used in 8+ locations.
- `pool.begin_with("BEGIN IMMEDIATE").await` -- used in 2 locations (skill trust, concurrent write safety). This is SQLite-specific.

### 2.3 SQL Dialect Differences

| Feature | SQLite | PostgreSQL | Compatibility |
|---------|--------|------------|---------------|
| Auto-increment PK | `INTEGER PRIMARY KEY AUTOINCREMENT` | `BIGSERIAL PRIMARY KEY` or `GENERATED ALWAYS AS IDENTITY` | Incompatible DDL |
| Timestamp default | `DEFAULT (datetime('now'))` | `DEFAULT now()` or `DEFAULT CURRENT_TIMESTAMP` | Incompatible DDL |
| Inline timestamp | `datetime('now')` in DML | `now()` or `CURRENT_TIMESTAMP` | Both support `CURRENT_TIMESTAMP` |
| Upsert | `ON CONFLICT(...) DO UPDATE` | `ON CONFLICT(...) DO UPDATE` | Compatible |
| `RETURNING` | Supported (SQLite 3.35+) | Supported | Compatible |
| FTS | FTS5 virtual tables + triggers | `tsvector`/`tsquery` + GIN index | Incompatible |
| JSON storage | `TEXT` + `json_extract()` | `JSONB` + `->>`/`@>` operators | Incompatible |
| Boolean | `INTEGER` (0/1) | Native `BOOLEAN` | sqlx handles mapping |
| BLOB | `BLOB` | `BYTEA` | sqlx handles mapping |
| Collation | `COLLATE NOCASE` | `LOWER()` or `citext` extension | Incompatible DDL |
| `INSERT OR REPLACE` | Supported | `INSERT ... ON CONFLICT DO UPDATE` | Rewrite needed |
| `INSERT OR IGNORE` | Supported | `INSERT ... ON CONFLICT DO NOTHING` | Rewrite needed |
| Bind placeholder | `?` | `$1`, `$2`, ... | **Critical incompatibility** |
| `PRAGMA` statements | Yes | No | SQLite-only |
| `BEGIN IMMEDIATE` | Yes | Not needed (MVCC) | SQLite-only |

### 2.4 Bind Placeholder Problem

This is the most pervasive incompatibility. SQLite uses `?` for all bind parameters while
PostgreSQL uses `$1, $2, ...` numbered placeholders. Every SQL string in the codebase
(hundreds of queries) uses `?`. Options:

1. **sqlx `Any` backend**: normalizes to `?` placeholders at runtime.
2. **Dual query modules**: maintain separate SQL per backend (doubles query count).
3. **Query rewriter**: transform `?` to `$N` at runtime.
4. **Conditional compilation**: `#[cfg(feature)]` on each query.

---

## 3. Architectural Decision: Feature-Flag Conditional Compilation

### 3.1 Rejected Alternative: `sqlx::Any` Runtime Backend

The `sqlx::Any` backend provides runtime database dispatch by erasing the concrete
`Database` type. It was evaluated and **rejected** for the following reasons:

- **No FTS support**: `AnyPool` cannot execute SQLite FTS5 or PostgreSQL `tsvector`
  queries. FTS is critical (messages, graph entities).
- **Limited type mapping**: `Any` normalizes types to the lowest common denominator.
  PostgreSQL `JSONB`, `BYTEA`, `TIMESTAMPTZ` are not representable in `AnyRow`.
- **No `PRAGMA` passthrough**: SQLite `PRAGMA` statements cannot be issued through `Any`.
- **No `BEGIN IMMEDIATE`**: Transaction mode control is lost.
- **Performance overhead**: Every query goes through an additional dispatch layer.
- **Migration complexity**: `sqlx::migrate!` still requires knowing the backend at
  compile time for `Any` pools.

### 3.2 Chosen Approach: Generics + Traits with Feature-Flag Selection

**Amendment 2 [2026-03-28]**: Use a **`DatabaseDriver` trait** that unifies a sqlx
`Database` type, a `Dialect`, and connection/migration logic into a single generic
parameter. Consumer crates are parameterized over `D: DatabaseDriver`. The active
backend is selected at compile time via a feature flag that resolves the
`ActiveDriver` type alias. Business logic stays unified; only SQL fragments
(via `Dialect` associated constants) and pool construction differ.

**Rationale**:
- Zero runtime overhead (monomorphization).
- Compile-time guarantees: if it builds with `--features postgres`, all queries
  are valid for PostgreSQL.
- Preserves SQLite-specific optimizations (WAL, PRAGMA, FTS5) when `sqlite` is
  selected.
- Allows PostgreSQL-specific optimizations (JSONB, GIN, pg_trgm) when `postgres`
  is selected.
- No query duplication: shared SQL uses `CURRENT_TIMESTAMP` and `ON CONFLICT`;
  dialect-specific fragments are provided by the `Dialect` trait.

---

## 4. Trait and Type Design

### 4.1 New Crate: `zeph-db`

Introduce a new **Layer 0** crate `zeph-db` that provides the database abstraction.
All crates that currently depend on sqlx directly will instead depend on `zeph-db`.

**Constitution compliance**: Layer 0 crate with no zeph-* dependencies. `zeph-memory`,
`zeph-scheduler`, `zeph-mcp`, `zeph-orchestration`, `zeph-index` all move their sqlx
dependency to transit through `zeph-db`.

```
zeph-db (Layer 0)
├── src/
│   ├── lib.rs           -- re-exports, ActiveDriver alias, DbPool/DbRow/DbTransaction aliases
│   ├── dialect.rs       -- Dialect trait + Sqlite + Postgres marker types
│   ├── driver.rs        -- DatabaseDriver trait definition
│   ├── driver/
│   │   ├── sqlite.rs    -- SqliteDriver (feature = "sqlite")
│   │   └── postgres.rs  -- PostgresDriver (feature = "postgres")
│   ├── bounds.rs        -- FullDriver blanket super-trait (reduces bound repetition)
│   ├── pool.rs          -- DbConfig, redact_url
│   ├── migrate.rs       -- Migration runner (delegates to driver)
│   ├── fts.rs           -- FTS sanitization (cfg-gated, text-level, not type-level)
│   ├── transaction.rs   -- begin/begin_write convenience wrappers
│   └── error.rs         -- Unified database error type
├── migrations/
│   ├── sqlite/          -- SQLite-specific migrations
│   └── postgres/        -- PostgreSQL-specific migrations
└── Cargo.toml
```

### 4.2 Feature Flag Design

**Amendment [2026-03-28]**: Both `sqlite` and `postgres` are non-default features.
The root `Cargo.toml` `default` feature explicitly includes `zeph-db/sqlite`. This
avoids the problem where `cargo test --all-features` or `cargo clippy --all-features`
would fail: with both features activated, the `compile_error!` fires intentionally.
`--all-features` is not a supported build mode for this workspace; use `--features full`
or `--features full,postgres` instead. This is documented in CI configuration.

```toml
# zeph-db/Cargo.toml
[features]
# NOTE: no default features. Both backends are opt-in.
# The root Cargo.toml default includes zeph-db/sqlite.
sqlite = ["sqlx/sqlite"]
postgres = ["sqlx/postgres"]
```

```toml
# Root Cargo.toml
[features]
default = ["bundled-skills", "scheduler", "guardrail", "zeph-db/sqlite"]
postgres = ["zeph-db/postgres"]
# NOTE: --all-features activates both sqlite and postgres, triggering compile_error!.
# This is intentional. CI and developers must use --features full or --features full,postgres.
```

**Mutual exclusivity**: `sqlite` and `postgres` are mutually exclusive at compile
time. The root binary selects exactly one. This is enforced by a `compile_error!`
in `lib.rs` if both or neither are enabled.

```rust
// zeph-db/src/lib.rs
#[cfg(all(feature = "sqlite", feature = "postgres"))]
compile_error!("features `sqlite` and `postgres` are mutually exclusive");

#[cfg(not(any(feature = "sqlite", feature = "postgres")))]
compile_error!("exactly one of `sqlite` or `postgres` must be enabled");
```

### 4.3 The `Dialect` Trait

**Amendment 2 [2026-03-28]**: Replaced the `#[cfg]`-gated `Dialect` struct with a
proper trait. The `Dialect` trait defines SQL fragment substitution as associated
constants and methods. Each backend provides a concrete zero-sized type implementing
the trait. This enables generic code to be parameterized over the dialect without
`#[cfg]` gates in business logic.

```rust
// zeph-db/src/dialect.rs

/// SQL fragments that differ between database backends.
///
/// Implemented by zero-sized marker types (`Sqlite`, `Postgres`).
/// All associated constants are `&'static str` for zero-cost usage.
pub trait Dialect: Send + Sync + 'static {
    /// The `NOW()` expression for this backend.
    ///
    /// `Sqlite`: `datetime('now')`
    /// `Postgres`: `now()`
    const NOW: &'static str;

    /// Auto-increment primary key DDL fragment.
    ///
    /// `Sqlite`: `INTEGER PRIMARY KEY AUTOINCREMENT`
    /// `Postgres`: `BIGSERIAL PRIMARY KEY`
    const AUTO_PK: &'static str;

    /// `INSERT OR IGNORE` prefix for this backend.
    ///
    /// `Sqlite`: `INSERT OR IGNORE`
    /// `Postgres`: `INSERT` (pair with `CONFLICT_NOTHING` suffix)
    const INSERT_IGNORE: &'static str;

    /// Suffix for conflict-do-nothing semantics.
    ///
    /// `Sqlite`: empty string (handled by `INSERT OR IGNORE` prefix)
    /// `Postgres`: `ON CONFLICT DO NOTHING`
    const CONFLICT_NOTHING: &'static str;

    /// Case-insensitive comparison expression for a column.
    ///
    /// `Sqlite`: `{col} COLLATE NOCASE`
    /// `Postgres`: `LOWER({col})`
    fn ilike(col: &str) -> String;
}

/// SQLite dialect marker type.
pub struct Sqlite;

impl Dialect for Sqlite {
    const NOW: &'static str = "datetime('now')";
    const AUTO_PK: &'static str = "INTEGER PRIMARY KEY AUTOINCREMENT";
    const INSERT_IGNORE: &'static str = "INSERT OR IGNORE";
    const CONFLICT_NOTHING: &'static str = "";

    fn ilike(col: &str) -> String {
        format!("{col} COLLATE NOCASE")
    }
}

/// PostgreSQL dialect marker type.
pub struct Postgres;

impl Dialect for Postgres {
    const NOW: &'static str = "now()";
    const AUTO_PK: &'static str = "BIGSERIAL PRIMARY KEY";
    const INSERT_IGNORE: &'static str = "INSERT";
    const CONFLICT_NOTHING: &'static str = "ON CONFLICT DO NOTHING";

    fn ilike(col: &str) -> String {
        format!("LOWER({col})")
    }
}
```

**Note**: `Dialect::bool_val()` is intentionally absent. sqlx's `Encode`
implementation handles bool→integer mapping for SQLite automatically. Callers
bind `bool` values directly.

### 4.4 The `DatabaseDriver` Trait

**Amendment 2 [2026-03-28]**: Introduced `DatabaseDriver` as the unified type-level
bridge between a sqlx `Database` type and the corresponding `Dialect`. This trait
is the single point where backend identity is resolved. Consumer crates parameterize
their stores over `D: DatabaseDriver` and never reference `sqlx::Sqlite` or
`sqlx::Postgres` directly.

```rust
// zeph-db/src/driver.rs

use crate::{Dialect, error::DbError};

/// Unifies a sqlx `Database` type with its `Dialect` and connection logic.
///
/// Each backend (`SqliteDriver`, `PostgresDriver`) implements this trait once.
/// Consumer crates use `D: DatabaseDriver` as their single generic parameter,
/// which gives access to both `D::Database` (for sqlx pool/query bounds) and
/// `D::Dialect` (for SQL fragment substitution).
pub trait DatabaseDriver: Send + Sync + 'static {
    /// The sqlx `Database` type (e.g., `sqlx::Sqlite`, `sqlx::Postgres`).
    type Database: sqlx::Database;

    /// The dialect providing SQL fragment constants.
    type Dialect: Dialect;

    /// Connect to the database and return a pool.
    ///
    /// # Errors
    ///
    /// Returns [`DbError`] if connection fails.
    fn connect(
        url: &str,
        max_connections: u32,
    ) -> impl std::future::Future<Output = Result<sqlx::Pool<Self::Database>, DbError>> + Send;

    /// Run all pending migrations.
    ///
    /// # Errors
    ///
    /// Returns [`DbError`] if any migration fails.
    fn run_migrations(
        pool: &sqlx::Pool<Self::Database>,
    ) -> impl std::future::Future<Output = Result<(), DbError>> + Send;

    /// Begin a standard deferred transaction.
    fn begin(
        pool: &sqlx::Pool<Self::Database>,
    ) -> impl std::future::Future<
        Output = Result<sqlx::Transaction<'_, Self::Database>, sqlx::Error>,
    > + Send;

    /// Begin a write-intent transaction.
    ///
    /// `Sqlite`: issues `BEGIN IMMEDIATE` to acquire the write lock upfront.
    /// `Postgres`: issues a standard `BEGIN` (MVCC handles concurrency).
    fn begin_write(
        pool: &sqlx::Pool<Self::Database>,
    ) -> impl std::future::Future<
        Output = Result<sqlx::Transaction<'_, Self::Database>, sqlx::Error>,
    > + Send;
}
```

### 4.4.1 `SqliteDriver`

```rust
// zeph-db/src/driver/sqlite.rs (compiled only with feature = "sqlite")

use crate::{dialect::Sqlite, error::DbError, DatabaseDriver};

pub struct SqliteDriver;

impl DatabaseDriver for SqliteDriver {
    type Database = sqlx::Sqlite;
    type Dialect = Sqlite;

    async fn connect(url: &str, max_connections: u32) -> Result<sqlx::SqlitePool, DbError> {
        use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
        use std::str::FromStr;

        let conn_url = if url == ":memory:" {
            "sqlite::memory:".to_string()
        } else {
            if let Some(parent) = std::path::Path::new(url).parent()
                && !parent.as_os_str().is_empty()
            {
                std::fs::create_dir_all(parent)?;
            }
            format!("sqlite:{url}?mode=rwc")
        };

        let opts = SqliteConnectOptions::from_str(&conn_url)
            .map_err(DbError::Sqlx)?
            .create_if_missing(true)
            .foreign_keys(true)
            .busy_timeout(std::time::Duration::from_secs(5))
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .synchronous(sqlx::sqlite::SqliteSynchronous::Normal);

        let pool = SqlitePoolOptions::new()
            .max_connections(max_connections)
            .connect_with(opts)
            .await
            .map_err(DbError::Sqlx)?;

        Ok(pool)
    }

    async fn run_migrations(pool: &sqlx::SqlitePool) -> Result<(), DbError> {
        sqlx::migrate!("./migrations/sqlite")
            .run(pool)
            .await
            .map_err(DbError::Sqlx)?;
        Ok(())
    }

    async fn begin(
        pool: &sqlx::SqlitePool,
    ) -> Result<sqlx::Transaction<'_, sqlx::Sqlite>, sqlx::Error> {
        pool.begin().await
    }

    async fn begin_write(
        pool: &sqlx::SqlitePool,
    ) -> Result<sqlx::Transaction<'_, sqlx::Sqlite>, sqlx::Error> {
        pool.begin_with("BEGIN IMMEDIATE").await
    }
}
```

### 4.4.2 `PostgresDriver`

```rust
// zeph-db/src/driver/postgres.rs (compiled only with feature = "postgres")

use crate::{dialect::Postgres, error::DbError, pool::redact_url, DatabaseDriver};

pub struct PostgresDriver;

impl DatabaseDriver for PostgresDriver {
    type Database = sqlx::Postgres;
    type Dialect = Postgres;

    async fn connect(url: &str, max_connections: u32) -> Result<sqlx::PgPool, DbError> {
        use sqlx::postgres::PgPoolOptions;

        let pool = PgPoolOptions::new()
            .max_connections(max_connections)
            .acquire_timeout(std::time::Duration::from_secs(30))
            .connect(url)
            .await
            .map_err(|e| DbError::Connection {
                url: redact_url(url).unwrap_or_else(|| "[redacted]".into()),
                source: e,
            })?;

        Ok(pool)
    }

    async fn run_migrations(pool: &sqlx::PgPool) -> Result<(), DbError> {
        sqlx::migrate!("./migrations/postgres")
            .run(pool)
            .await
            .map_err(DbError::Sqlx)?;
        Ok(())
    }

    async fn begin(
        pool: &sqlx::PgPool,
    ) -> Result<sqlx::Transaction<'_, sqlx::Postgres>, sqlx::Error> {
        pool.begin().await
    }

    async fn begin_write(
        pool: &sqlx::PgPool,
    ) -> Result<sqlx::Transaction<'_, sqlx::Postgres>, sqlx::Error> {
        // PostgreSQL uses MVCC; standard BEGIN is sufficient.
        // For write-exclusion semantics, callers must use
        // SELECT ... FOR UPDATE inside the transaction.
        pool.begin().await
    }
}
```

### 4.4.3 Convenience Type Aliases

**Amendment 2 [2026-03-28]**: `#[cfg]`-gated type aliases are retained as **ergonomic
shortcuts** that resolve to the active driver's associated types. These are the only
`#[cfg]`-gated type definitions in `zeph-db`. All generic code must use
`D: DatabaseDriver` bounds, not these aliases.

```rust
// zeph-db/src/lib.rs

/// The active database driver, selected at compile time.
#[cfg(feature = "sqlite")]
pub type ActiveDriver = driver::SqliteDriver;
#[cfg(feature = "postgres")]
pub type ActiveDriver = driver::PostgresDriver;

/// Convenience alias: pool for the active backend.
pub type DbPool = sqlx::Pool<<ActiveDriver as DatabaseDriver>::Database>;

/// Convenience alias: row for the active backend.
pub type DbRow = <<ActiveDriver as DatabaseDriver>::Database as sqlx::Database>::Row;

/// Convenience alias: query result for the active backend.
pub type DbQueryResult =
    <<ActiveDriver as DatabaseDriver>::Database as sqlx::Database>::QueryResult;

/// Convenience alias: transaction for the active backend.
pub type DbTransaction<'a> =
    sqlx::Transaction<'a, <ActiveDriver as DatabaseDriver>::Database>;

/// Convenience alias: the active dialect.
pub type ActiveDialect = <ActiveDriver as DatabaseDriver>::Dialect;

/// Re-export sqlx query builders.
pub use sqlx::{query, query_as, query_scalar, Row, FromRow, Executor, Error as SqlxError};
```

### 4.4.4 Generic Store Pattern

Consumer crates parameterize their store types over `D: DatabaseDriver`. The
existing `DbStore` (formerly `SqliteStore`) and `SqliteVectorStore` become generic:

```rust
// Example: zeph-memory store becomes generic over the driver.

use std::marker::PhantomData;
use zeph_db::DatabaseDriver;

/// Database-backed memory store, generic over the backend.
#[derive(Debug, Clone)]
pub struct Store<D: DatabaseDriver> {
    pool: sqlx::Pool<D::Database>,
    _driver: PhantomData<D>,
}

impl<D: DatabaseDriver> Store<D>
where
    for<'c> &'c mut <D::Database as sqlx::Database>::Connection:
        sqlx::Executor<'c, Database = D::Database>,
{
    /// Wrap an existing pool.
    pub fn from_pool(pool: sqlx::Pool<D::Database>) -> Self {
        Self {
            pool,
            _driver: PhantomData,
        }
    }

    /// Access the underlying pool.
    pub fn pool(&self) -> &sqlx::Pool<D::Database> {
        &self.pool
    }
}

/// Backward-compatible alias for the active backend.
pub type DbStore = Store<zeph_db::ActiveDriver>;
/// Legacy alias.
pub type SqliteStore = DbStore;
```

**Required sqlx trait bounds** for generic query methods on `Store<D>`:

```rust
// When a method needs to execute queries, the impl block requires:
impl<D: DatabaseDriver> Store<D>
where
    for<'q> <D::Database as sqlx::database::HasArguments<'q>>::Arguments:
        sqlx::IntoArguments<'q, D::Database>,
    for<'c> &'c mut <D::Database as sqlx::Database>::Connection:
        sqlx::Executor<'c, Database = D::Database>,
    // Additional bounds as needed for specific column types:
    // i64: sqlx::Type<D::Database> + for<'q> sqlx::Encode<'q, D::Database>,
    // String: sqlx::Type<D::Database> + for<'q> sqlx::Encode<'q, D::Database>,
    // etc.
{
    // query methods here
}
```

**Simplification strategy**: Because sqlx's `Sqlite` and `Postgres` types both
satisfy these bounds for all standard Rust types (`i64`, `String`, `bool`, `Vec<u8>`,
`Option<T>`), the bounds can be collected into a single **blanket super-trait** to
avoid repeating them on every impl block:

```rust
// zeph-db/src/bounds.rs

/// Marker trait automatically implemented for all `DatabaseDriver` types
/// whose `Database` supports standard Rust types in queries.
///
/// This trait exists solely to reduce bound repetition on generic impl blocks.
/// It is sealed and cannot be implemented outside `zeph-db`.
pub trait FullDriver: DatabaseDriver
where
    for<'q> <Self::Database as sqlx::database::HasArguments<'q>>::Arguments:
        sqlx::IntoArguments<'q, Self::Database>,
    for<'c> &'c mut <Self::Database as sqlx::Database>::Connection:
        sqlx::Executor<'c, Database = Self::Database>,
    i64: for<'q> sqlx::Encode<'q, Self::Database> + sqlx::Type<Self::Database>,
    String: for<'q> sqlx::Encode<'q, Self::Database> + sqlx::Type<Self::Database>,
    bool: for<'q> sqlx::Encode<'q, Self::Database> + sqlx::Type<Self::Database>,
    Vec<u8>: for<'q> sqlx::Encode<'q, Self::Database> + sqlx::Type<Self::Database>,
{}

impl FullDriver for crate::driver::SqliteDriver {}
#[cfg(feature = "postgres")]
impl FullDriver for crate::driver::PostgresDriver {}
```

**Migration path**: The type aliases (`DbStore`, `SqliteStore`) ensure that
existing consumer code compiles unchanged during Phase 1. In Phase 2, consumer
crates progressively adopt `Store<D>` generics where multi-backend support is
needed.

### 4.4.5 `VectorStore` Generics

```rust
// zeph-memory/src/db_vector_store.rs

use std::marker::PhantomData;
use zeph_db::DatabaseDriver;

/// Database-backed vector store, generic over the backend.
pub struct DbVectorStore<D: DatabaseDriver> {
    pool: sqlx::Pool<D::Database>,
    _driver: PhantomData<D>,
}

impl<D: DatabaseDriver> DbVectorStore<D> {
    pub fn new(pool: sqlx::Pool<D::Database>) -> Self {
        Self {
            pool,
            _driver: PhantomData,
        }
    }
}

/// Backward-compatible alias.
pub type SqliteVectorStore = DbVectorStore<zeph_db::ActiveDriver>;
```

### 4.5 Bind Placeholder Strategy

Rather than rewriting every query string, use `sqlx`'s built-in placeholder
normalization. **Key insight**: `sqlx::query()` on `PgPool` accepts `$1` style
placeholders, while on `SqlitePool` it accepts `?`. Since `DbPool` is a type alias,
the correct placeholder style is enforced at compile time.

**Strategy**: Introduce a `sql!` procedural macro (or a simpler `format_sql!` macro)
that converts `?` to `$N` when the `postgres` feature is active:

```rust
// zeph-db/src/lib.rs

/// Convert `?` placeholders to `$N` for PostgreSQL.
///
/// At compile time with `sqlite` feature, this is a no-op identity.
/// At compile time with `postgres` feature, replaces `?` with `$1`, `$2`, etc.
#[cfg(feature = "sqlite")]
#[macro_export]
macro_rules! sql {
    ($query:expr) => { $query };
}

#[cfg(feature = "postgres")]
#[macro_export]
macro_rules! sql {
    ($query:expr) => {{
        // Compile-time string transformation via const fn is not yet stable for
        // complex operations. Use a lazy_static or once_cell cached rewrite.
        $crate::rewrite_placeholders($query)
    }};
}

/// Rewrite `?` bind markers to `$1, $2, ...` for PostgreSQL.
///
/// Skips `?` inside single-quoted string literals.
pub fn rewrite_placeholders(query: &str) -> String {
    let mut out = String::with_capacity(query.len() + 16);
    let mut n = 0u32;
    let mut in_string = false;
    for ch in query.chars() {
        match ch {
            '\'' => {
                in_string = !in_string;
                out.push(ch);
            }
            '?' if !in_string => {
                n += 1;
                out.push('$');
                out.push_str(&n.to_string());
            }
            _ => out.push(ch),
        }
    }
    out
}
```

**Amendment [2026-03-28]**: The `sql!` macro and query statics use conditional
compilation to avoid unnecessary heap allocation on the SQLite path. For SQLite,
query statics are `&'static str` constants (zero allocation). For PostgreSQL,
`LazyLock<String>` caches the rewritten query on first access.

Additionally, PostgreSQL JSONB queries that use `?`, `?|`, or `?&` operators must
**not** pass through `sql!()` or `rewrite_placeholders()`, because these operators
use `?` as a JSONB key-existence check, not as a bind placeholder. Such queries
must use `$N` placeholders directly and be annotated with a safety comment:

```rust
// SAFETY: uses PG JSONB operators (?/??|/?&), not bind placeholders.
// This query is PostgreSQL-only and uses $N placeholders directly.
```

**Correct query static pattern**:

```rust
#[cfg(feature = "sqlite")]
const LOAD_HISTORY_SQL: &str =
    "SELECT role, content, parts, agent_visible, user_visible, id FROM (\
     SELECT role, content, parts, agent_visible, user_visible, id FROM messages \
     WHERE conversation_id = ? AND deleted_at IS NULL \
     ORDER BY id DESC \
     LIMIT ?\
    ) ORDER BY id ASC";

#[cfg(feature = "postgres")]
static LOAD_HISTORY_SQL: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    rewrite_placeholders(
        "SELECT role, content, parts, agent_visible, user_visible, id FROM (\
         SELECT role, content, parts, agent_visible, user_visible, id FROM messages \
         WHERE conversation_id = ? AND deleted_at IS NULL \
         ORDER BY id DESC \
         LIMIT ?\
        ) ORDER BY id ASC"
    )
});
```

For the SQLite feature, `sql!()` returns the literal `&str` directly with zero
allocation. For PostgreSQL, `LazyLock` ensures the rewrite runs exactly once.
The previous claim that "the optimizer eliminates `LazyLock`" on SQLite was
incorrect -- `LazyLock<String>` always heap-allocates.

### 4.6 Pool Construction

**Amendment 2 [2026-03-28]**: Connection logic has moved into `DatabaseDriver::connect()`
(sections 4.4.1, 4.4.2). `DbConfig` is now a thin configuration holder that delegates
to `ActiveDriver::connect()` and `ActiveDriver::run_migrations()`.

```rust
// zeph-db/src/pool.rs

use crate::{ActiveDriver, DatabaseDriver, DbPool, error::DbError};

pub struct DbConfig {
    /// Database URL. Sqlite: file path or `:memory:`. Postgres: connection URL.
    pub url: String,
    /// Maximum number of connections in the pool.
    pub max_connections: u32,
    /// Sqlite only: maximum write-pool connections. Default 1.
    ///
    /// Sqlite WAL allows only one concurrent writer; a write pool > 1
    /// creates unnecessary SQLITE_BUSY contention.
    ///
    /// **Amendment 1 [2026-03-28]**: Added to prevent SQLITE_BUSY stalls.
    pub write_pool_size: u32,
}

impl Default for DbConfig {
    fn default() -> Self {
        Self {
            url: String::new(),
            max_connections: 5,
            write_pool_size: 1,
        }
    }
}

impl DbConfig {
    /// Connect to the database and run migrations.
    ///
    /// Delegates to `ActiveDriver::connect()` and `ActiveDriver::run_migrations()`.
    ///
    /// # Errors
    ///
    /// Returns [`DbError`] if connection or migration fails.
    pub async fn connect(&self) -> Result<DbPool, DbError> {
        let pool = ActiveDriver::connect(&self.url, self.max_connections).await?;
        ActiveDriver::run_migrations(&pool).await?;

        // Sqlite-specific post-migration optimization.
        #[cfg(feature = "sqlite")]
        if self.url != ":memory:" {
            sqlx::query("PRAGMA wal_checkpoint(PASSIVE)")
                .execute(&pool)
                .await
                .map_err(DbError::Sqlx)?;
        }

        Ok(pool)
    }
}

/// Strip password from a database URL for safe logging.
///
/// **Amendment 1 [2026-03-28]**: Applied to all log output, error messages,
/// and TUI display of the connection URL. Replaces `://user:password@` with
/// `://[redacted]@`.
///
/// Returns `None` if the URL contains no embedded credentials (already safe).
pub fn redact_url(url: &str) -> Option<String> {
    let re = regex::Regex::new(r"://[^:]+:[^@]+@").ok()?;
    if re.is_match(url) {
        Some(re.replace(url, "://[redacted]@").into_owned())
    } else {
        None
    }
}
```

### 4.7 Transaction Abstraction

**Amendment 2 [2026-03-28]**: Transaction functions are now methods on
`DatabaseDriver` (see sections 4.4.1 and 4.4.2). The `DbTransaction` type alias
and free functions `begin()` / `begin_write()` are retained as convenience
wrappers that delegate to `ActiveDriver`:

```rust
// zeph-db/src/transaction.rs

use crate::{ActiveDriver, DatabaseDriver, DbPool};

pub type DbTransaction<'a> =
    sqlx::Transaction<'a, <ActiveDriver as DatabaseDriver>::Database>;

/// Begin a standard deferred transaction.
pub async fn begin(pool: &DbPool) -> Result<DbTransaction<'_>, sqlx::Error> {
    ActiveDriver::begin(pool).await
}

/// Begin a write-intent transaction.
///
/// Sqlite: `BEGIN IMMEDIATE` to acquire write lock upfront.
/// Postgres: standard `BEGIN` (MVCC handles concurrency).
pub async fn begin_write(pool: &DbPool) -> Result<DbTransaction<'_>, sqlx::Error> {
    ActiveDriver::begin_write(pool).await
}
```

Generic code that operates over `D: DatabaseDriver` calls `D::begin()` and
`D::begin_write()` directly instead of these free functions.

**Amendment 1 [2026-03-28]**: On PostgreSQL, `begin_write()` returns a standard
`BEGIN` transaction (MVCC handles concurrency). However, the two `BEGIN IMMEDIATE`
locations in `skills.rs` (skill trust score updates) rely on write-exclusion
semantics to prevent lost updates. On PostgreSQL, the equivalent pattern is
`SELECT ... FOR UPDATE` inside the transaction to acquire a row-level lock before
reading and updating the trust score. This is a **required implementation note
for Phase 2**: every `begin_write()` call site in `skills.rs` must be rewritten to
use `SELECT skill_name, trust_score FROM skill_trust WHERE skill_name = $1 FOR UPDATE`
before the subsequent `UPDATE` statement. Without this, concurrent trust score
updates produce a lost-update race under PostgreSQL's default READ COMMITTED
isolation.

---

## 5. Migration Strategy

### 5.1 Separate Migration Directories

Maintain **two sets** of migration files: `migrations/sqlite/` and `migrations/postgres/`.
These share the same logical schema evolution but use backend-specific DDL.

```
crates/zeph-db/
├── migrations/
│   ├── sqlite/
│   │   ├── 001_init.sql
│   │   ├── 002_embeddings_metadata.sql
│   │   ├── ...
│   │   └── 049_mem_scenes.sql
│   └── postgres/
│       ├── 001_init.sql
│       ├── 002_embeddings_metadata.sql
│       ├── ...
│       └── 049_mem_scenes.sql
```

**Rationale**: A single set of migrations with conditional dialect is fragile and hard
to maintain. Two sets allow full use of each backend's capabilities:

- SQLite migrations use FTS5, `AUTOINCREMENT`, `datetime('now')` defaults.
- PostgreSQL migrations use `BIGSERIAL`, `TIMESTAMPTZ`, `tsvector`/GIN, `JSONB`.

The `sqlx::migrate!` macro requires a compile-time path, so:

```rust
// zeph-db/src/migrate.rs

#[cfg(feature = "sqlite")]
pub async fn run_migrations(pool: &DbPool) -> Result<(), sqlx::Error> {
    sqlx::migrate!("./migrations/sqlite").run(pool).await?;
    Ok(())
}

#[cfg(feature = "postgres")]
pub async fn run_migrations(pool: &DbPool) -> Result<(), sqlx::Error> {
    sqlx::migrate!("./migrations/postgres").run(pool).await?;
    Ok(())
}
```

### 5.2 Migration Porting Guide

For each existing SQLite migration, create a PostgreSQL equivalent:

| SQLite | PostgreSQL Equivalent |
|--------|----------------------|
| `INTEGER PRIMARY KEY AUTOINCREMENT` | `BIGSERIAL PRIMARY KEY` |
| `DEFAULT (datetime('now'))` | `DEFAULT now()` |
| `TEXT NOT NULL DEFAULT '[]'` | `JSONB NOT NULL DEFAULT '[]'::jsonb` |
| `TEXT NOT NULL DEFAULT '{}'` | `JSONB NOT NULL DEFAULT '{}'::jsonb` |
| `REAL` | `DOUBLE PRECISION` |
| `BLOB` | `BYTEA` |
| `CREATE VIRTUAL TABLE ... USING fts5(...)` | `ALTER TABLE ADD COLUMN tsv tsvector; CREATE INDEX ... USING GIN(tsv);` + trigger |
| `COLLATE NOCASE` | Use `citext` extension or `LOWER()` |
| FTS5 sync triggers | `tsvector_update_trigger` |
| `PRAGMA` statements | Omit (not applicable) |
| `INSERT OR IGNORE` | `INSERT ... ON CONFLICT DO NOTHING` |
| `INSERT OR REPLACE` | `INSERT ... ON CONFLICT(...) DO UPDATE SET ...` |

### 5.3 Full-Text Search Abstraction

FTS is used in two places:
1. `messages_fts` -- keyword search over message content.
2. `graph_entities_fts` -- fuzzy entity lookup.

Since FTS syntax is completely different between backends, wrap it in a
dialect-specific module:

```rust
// zeph-db/src/fts.rs

/// Build a full-text search condition for the messages table.
///
/// SQLite: `messages_fts MATCH ?`
/// PostgreSQL: `messages.tsv @@ plainto_tsquery('english', $1)`
#[cfg(feature = "sqlite")]
pub fn messages_fts_match() -> &'static str {
    "messages_fts MATCH ?"
}

#[cfg(feature = "postgres")]
pub fn messages_fts_match() -> &'static str {
    "messages.tsv @@ plainto_tsquery('english', $1)"
}

/// Sanitize a user query for safe FTS usage.
///
/// SQLite: strip FTS5 special characters.
/// PostgreSQL: use `plainto_tsquery` which handles sanitization.
#[cfg(feature = "sqlite")]
pub fn sanitize_fts_query(query: &str) -> String {
    // existing sanitize_fts5_query logic
    query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(feature = "postgres")]
pub fn sanitize_fts_query(query: &str) -> String {
    // PostgreSQL's plainto_tsquery handles most sanitization,
    // but strip obvious injection attempts.
    query.replace('\'', "''")
}
```

---

## 6. Configuration Design

### 6.1 TOML Config Structure

```toml
[memory]
# Existing field, renamed for clarity. Old `sqlite_path` is a migration alias.
database_url = ".zeph/data/zeph.db"
# New field: "sqlite" (default) or "postgres"
database_backend = "sqlite"
# Existing field
sqlite_pool_size = 5
# New fields for PostgreSQL
# postgres_url = "postgres://user:pass@host:5432/zeph"  # or resolved from vault
# postgres_max_connections = 10
# postgres_ssl_mode = "prefer"  # "disable" | "prefer" | "require"
```

**Vault integration**: When `database_backend = "postgres"`, the connection URL
can reference a vault key:

```toml
[memory]
database_backend = "postgres"
# Resolved from vault at startup:
# ZEPH_DATABASE_URL → postgres://...
```

**Amendment [2026-03-28]**: Credential exposure prevention requirements:

1. `ZEPH_DATABASE_URL` is the canonical vault key for PostgreSQL credentials. It
   is resolved automatically from the age vault at startup, consistent with all
   other `ZEPH_*` keys.
2. If `postgres_url` contains an inline password (not from vault), emit a startup
   warning: "Connection URL contains embedded credentials. Consider using vault
   resolution (ZEPH_DATABASE_URL) instead."
3. The `redact_url()` function (see 4.6) must be applied to all log output,
   error messages, and TUI display of the connection URL.
4. `DbError::Connection` must store only the redacted URL, never the original.
5. Ensure the existing `zeph-core` redaction system (`RedactFilter`) covers
   `postgres(ql)?://` URLs via a regex pattern for `://[^:]+:[^@]+@`.

### 6.2 Config Types

```rust
// zeph-config/src/memory.rs (additions)

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DatabaseBackend {
    #[default]
    Sqlite,
    Postgres,
}

// MemoryConfig additions:
// pub database_backend: DatabaseBackend,
// pub postgres_url: Option<String>,
// pub postgres_max_connections: Option<u32>,
// pub postgres_ssl_mode: Option<String>,
```

### 6.3 Config Migration

Add a step to `--migrate-config` that:
1. Renames `sqlite_path` to `database_url` if `database_backend` is absent.
2. Adds `database_backend = "sqlite"` default.
3. Preserves all existing fields.

---

## 7. Feature Flag Design for Cargo.toml

### 7.1 Root Workspace

```toml
# Cargo.toml [workspace.dependencies]
zeph-db = { path = "crates/zeph-db", version = "0.17.1" }

# sqlx gains postgres feature option
sqlx = { version = "0.8", default-features = false }
```

### 7.2 `zeph-db` Crate

**Amendment [2026-03-28]**: Features updated per 4.2 amendment (no default).
`sqlx/macros` removed -- `zeph-db` uses zero `query!` macros, so the proc-macro
compilation adds unnecessary build time. Consumer crates that need `sqlx/macros`
can add it directly.

```toml
[package]
name = "zeph-db"
version.workspace = true
edition.workspace = true

[features]
# No default — root Cargo.toml selects the backend.
sqlite = ["sqlx/sqlite"]
postgres = ["sqlx/postgres"]

[dependencies]
# NOTE: "macros" deliberately excluded — zeph-db uses query() not query!().
# This saves ~5-15s on cold builds by avoiding sqlx-macros proc-macro compilation.
sqlx = { workspace = true, features = ["runtime-tokio-rustls", "migrate"] }
regex = { workspace = true }  # for redact_url()
thiserror.workspace = true
tokio = { workspace = true, features = ["rt"] }
tracing.workspace = true
```

### 7.3 Consumer Crates

Each crate that currently depends on `sqlx` directly changes to depend on `zeph-db`:

```toml
# crates/zeph-memory/Cargo.toml
[dependencies]
# REMOVE: sqlx = { workspace = true, features = ["macros", "runtime-tokio-rustls", "sqlite", "migrate"] }
# ADD:
zeph-db.workspace = true
```

The `sqlite` vs `postgres` feature is propagated from the root binary through
feature unification:

```toml
# Root Cargo.toml
[features]
default = ["bundled-skills", "scheduler", "guardrail", "zeph-db/sqlite"]
postgres = ["zeph-db/postgres"]
# sqlite is activated via default features. PostgreSQL is explicit opt-in.
```

### 7.4 Impact on Existing Features

**Amendment [2026-03-28]**: Updated for non-default feature design.

- `full` feature: includes `zeph-db/sqlite` via `default`. Unchanged behavior.
- New feature combination: `full,postgres --no-default-features` for PostgreSQL
  builds. This disables the default `zeph-db/sqlite` and activates `postgres`.
- `--all-features` is **not supported** and triggers `compile_error!`. This is
  intentional and documented. CI must use `--features full` or
  `--features full,postgres --no-default-features`.
- CI matrix: add a PostgreSQL job that builds with
  `--features full,postgres --no-default-features`.

---

## 8. Crate Structure Decision

### New `zeph-db` Crate (Recommended)

**Arguments for a dedicated crate**:

1. **Single source of truth** for pool construction, migrations, dialect, and
   type aliases. Without it, each consumer crate would need its own
   `#[cfg(feature)]` blocks for pool types.

2. **Layer 0 placement** avoids circular dependencies. `zeph-memory` (Layer 1)
   and `zeph-scheduler` (Layer 0) both need the abstraction. A Layer 0 crate
   that only wraps sqlx satisfies both.

3. **Migration consolidation**. Currently, `zeph-memory` owns the main migration
   directory while `zeph-scheduler` and `zeph-mcp` use inline `CREATE TABLE`.
   Moving all migrations to `zeph-db` unifies schema management.

4. **Feature flag isolation**. The `sqlite`/`postgres` toggle lives in one crate
   rather than being duplicated across 6 consumer crates.

**Arguments against** (and rebuttals):

- "Another crate increases workspace size" -- True, but the crate is small (~500 LOC)
  and consolidates scattered database logic.
- "Adds a dependency hop" -- Minimal impact; sqlx is already a transitive dependency.

### Layer Assignment

`zeph-db` is **Layer 0** (no zeph-* dependencies). Updated layering:

- **Layer 0**: `zeph-llm`, `zeph-a2a`, `zeph-gateway`, `zeph-scheduler`, `zeph-common`, **`zeph-db`**
- **Layer 1**: `zeph-memory` (-> llm, **db**), `zeph-tools` (-> common), `zeph-index` (-> llm, memory, **db**)
- Unchanged for Layers 2-4.

`zeph-scheduler` (Layer 0) depends on `zeph-db` (Layer 0). Same-layer imports are
prohibited by the constitution for *feature crates*, but `zeph-db` is an
infrastructure/utility crate analogous to `zeph-common`. The constitution should
be amended to explicitly allow infrastructure crate imports within the same layer.

---

## 9. Query Compatibility Strategy

### 9.1 Shared Queries (80% of total)

Most queries use standard SQL that works on both backends after placeholder rewriting:

```rust
// Before (SQLite only):
sqlx::query_as("SELECT role, content FROM messages WHERE conversation_id = ? LIMIT ?")
    .bind(cid)
    .bind(limit)
    .fetch_all(&self.pool)
    .await?;

// After (both backends):
use zeph_db::sql;

sqlx::query_as(sql!("SELECT role, content FROM messages WHERE conversation_id = ? LIMIT ?"))
    .bind(cid)
    .bind(limit)
    .fetch_all(&self.pool)
    .await?;
```

### 9.2 Dialect-Specific Queries (15% of total)

Queries that use `datetime('now')`, `INSERT OR REPLACE`, or `COLLATE NOCASE`:

```rust
// Before:
sqlx::query("UPDATE graph_edges SET expired_at = datetime('now') WHERE id = ?")

// After:
use zeph_db::Dialect;

sqlx::query(&format!(
    "UPDATE graph_edges SET expired_at = {} WHERE id = {}",
    Dialect::NOW,
    sql!("?")
))
```

For complex cases, use `format!` with dialect constants. The `sql!` macro handles
placeholder rewriting, and `Dialect::*` constants provide the SQL fragments.

### 9.3 Backend-Exclusive Queries (5% of total)

FTS queries, PRAGMA statements, and vector BLOB operations:

```rust
#[cfg(feature = "sqlite")]
async fn search_fts(&self, query: &str) -> Result<Vec<Message>, MemoryError> {
    let sanitized = fts::sanitize_fts_query(query);
    sqlx::query_as(
        "SELECT ... FROM messages JOIN messages_fts ON messages.id = messages_fts.rowid \
         WHERE messages_fts MATCH ?"
    )
    .bind(&sanitized)
    .fetch_all(&self.pool)
    .await?
}

#[cfg(feature = "postgres")]
async fn search_fts(&self, query: &str) -> Result<Vec<Message>, MemoryError> {
    let sanitized = fts::sanitize_fts_query(query);
    sqlx::query_as(
        "SELECT ... FROM messages \
         WHERE tsv @@ plainto_tsquery('english', $1)"
    )
    .bind(&sanitized)
    .fetch_all(&self.pool)
    .await?
}
```

---

## 10. Implementation Plan

### Phase 1: Foundation (non-breaking)

**Goal**: Introduce `zeph-db` crate and migrate `SqliteStore` to use it, with zero
behavioral changes for existing SQLite users.

1. Create `crates/zeph-db/` with:
   - `lib.rs`: type aliases, `sql!` macro, `compile_error!` guards.
   - `dialect.rs`: `Dialect` struct with SQLite-only constants initially.
   - `pool.rs`: `DbConfig` with SQLite connection logic extracted from `SqliteStore::new()`.
   - `migrate.rs`: Migration runner wrapping `sqlx::migrate!`.
   - `error.rs`: `DbError` enum wrapping `sqlx::Error`.
   - `fts.rs`: FTS helpers (SQLite only initially).

2. Move all 49 migration files from `crates/zeph-memory/migrations/` to
   `crates/zeph-db/migrations/sqlite/`.

3. Update `zeph-memory` to depend on `zeph-db` instead of sqlx directly.
   - Replace `SqlitePool` with `zeph_db::DbPool` in `SqliteStore`.
   - Replace `sqlx::migrate!("./migrations")` with `zeph_db::run_migrations()`.
   - Wrap all query strings with `sql!()` macro.

4. Update `zeph-scheduler`, `zeph-mcp`, `zeph-orchestration`, `zeph-index` similarly.
   - Move inline `CREATE TABLE` schemas to proper migrations in `zeph-db/migrations/sqlite/`.

5. Rename `SqliteStore` to `DbStore` (with a `pub type SqliteStore = DbStore` alias
   for backward compatibility within the crate).

6. All existing tests pass unchanged.

**Estimated scope**: ~1500 LOC changes, mostly mechanical `use` statement updates.

### Phase 2: PostgreSQL Backend

**Goal**: Add PostgreSQL support behind the `postgres` feature flag.

1. Add `postgres` feature to `zeph-db/Cargo.toml`.

2. Create `crates/zeph-db/migrations/postgres/` with all 49 migration equivalents.

3. Add PostgreSQL connection logic to `DbConfig::connect()`.

4. Add `#[cfg(feature = "postgres")]` variants for:
   - `Dialect` constants.
   - `sql!` macro (placeholder rewriting).
   - FTS helpers.
   - Transaction helpers.

5. Add `postgres` feature to root `Cargo.toml`.

6. Add PostgreSQL integration tests (behind `--ignored` flag, require running
   PostgreSQL instance).

7. Update `--init` wizard to offer backend selection.

8. Update documentation.

**Estimated scope**: ~2000 LOC new code, ~500 LOC modifications.

### Phase 3: Config and Tooling

**Goal**: Complete the user-facing integration.

1. Add `database_backend` and `postgres_*` fields to `MemoryConfig`.

2. Add `--migrate-config` step for the new fields.

3. Add vault key resolution for `ZEPH_DATABASE_URL`.

4. Add TUI config panel for backend selection.

5. Add CI matrix job for PostgreSQL builds.

6. Add `zeph db migrate` CLI subcommand for manual migration control.

---

## 11. Risks and Mitigations

### 11.1 Build Time Impact

**Risk**: Supporting two sqlx backends increases compile time.

**Mitigation**: Only one backend is compiled at a time (mutually exclusive features).
The `postgres` feature is never in `default` and only activated explicitly. No impact
on default builds.

### 11.2 SQLite Single-Writer vs PostgreSQL MVCC

**Risk**: Code that relies on SQLite's single-writer guarantee (e.g., `BEGIN IMMEDIATE`
for write exclusion) may behave differently under PostgreSQL's MVCC.

**Mitigation**: The `begin_write()` helper provides the appropriate transaction mode
per backend.

**Amendment [2026-03-28]**: For PostgreSQL, the two `BEGIN IMMEDIATE` locations in
`skills.rs` **must** use `SELECT ... FOR UPDATE` to acquire a row-level lock before
reading and updating skill trust scores. This is a required Phase 2 implementation
step, not an audit item. Without `FOR UPDATE`, concurrent trust score updates
produce a lost-update race under PostgreSQL's default READ COMMITTED isolation
(agent A and B both read trust_score = 0.8, then A writes 0.85, B overwrites to
0.75, discarding A's update). See section 4.7 amendment for the concrete pattern.

### 11.3 FTS Feature Parity

**Risk**: SQLite FTS5 and PostgreSQL `tsvector` have different ranking algorithms,
tokenizers, and query syntax. Search quality may differ between backends.

**Mitigation**: Accept divergence as inherent. Both backends provide "good enough"
full-text search for the agent's needs. The FTS abstraction module documents
behavioral differences. No attempt to make results identical.

### 11.4 Migration Drift

**Risk**: Two separate migration directories can drift out of sync.

**Mitigation**:
1. CI job that verifies SQLite and PostgreSQL migration directories have the same
   number of files with matching numeric prefixes.
2. **Amendment [2026-03-28]**: Strengthen the parity check beyond file count. In CI,
   run both migration sets against their respective backends and compare the resulting
   schema catalogs: `information_schema.columns` for PostgreSQL vs `pragma_table_info`
   for SQLite. Generate a normalized schema diff as a CI artifact. This is more
   robust than DDL text comparison and detects column type, constraint, and index
   divergence that file-count checks miss.
3. Convention: every PR that adds a SQLite migration must include the PostgreSQL
   equivalent (enforced by PR template checklist).

### 11.5 `sql!` Macro Edge Cases

**Risk**: The placeholder rewriter may incorrectly transform `?` inside string
literals, comments, or `??` escape sequences.

**Mitigation**: The rewriter tracks single-quote state to skip string literals.
Add comprehensive tests for edge cases (quoted `?`, `??`, multi-line queries,
nested subqueries).

**Amendment [2026-03-28]**: The previous claim that "PostgreSQL does not use `?`
for any other purpose" was **factually incorrect**. PostgreSQL uses `?` as a JSONB
key-existence operator, and `?|` / `?&` as array-based JSONB operators. These are
documented core operators. Any query using JSONB key-existence checks would have
`?` silently rewritten to `$N`, producing a malformed query.

**Resolution**: Queries that use PostgreSQL JSONB operators (`?`, `?|`, `?&`) must
**not** pass through `sql!()` or `rewrite_placeholders()`. Such queries must use
`$N` placeholders directly and are by definition PostgreSQL-only (behind
`#[cfg(feature = "postgres")]`). They must be annotated with:
```rust
// SAFETY: uses PG JSONB operators (?/??|/?&), not bind placeholders.
```

Additionally, `rewrite_placeholders()` does not handle dollar-quoted strings
(`$$...$$`) or SQL comments (`--`, `/* */`). These patterns are banned in shared
queries that pass through the rewriter. PostgreSQL-only queries that need them
must use `$N` placeholders directly.

### 11.6 Downstream Breakage

**Risk**: Renaming `SqliteStore` and changing pool types breaks downstream code
in `zeph-core` that references `SqliteStore` directly.

**Mitigation**: Phase 1 provides a type alias `pub type SqliteStore = DbStore` for
backward compatibility. Callers are migrated incrementally. The alias is removed
after all callers are updated (separate PR).

### 11.7 Vector Storage in SQLite

**Risk**: `SqliteVectorStore` stores vectors as BLOBs with in-memory cosine
similarity. PostgreSQL has `pgvector` extension for native vector operations.

**Mitigation**: `SqliteVectorStore` remains SQLite-only (it is an alternative to
Qdrant, not a primary store). When using PostgreSQL backend, vector storage goes
through Qdrant exclusively. The `vector_backend = "sqlite"` config option is
only valid when `database_backend = "sqlite"`.

---

## 12. Key Invariants

1. **Exactly one backend at compile time.** The `sqlite` and `postgres` features
   are mutually exclusive. A build with both enabled is a hard compile error.

2. **No `sqlx::Pool<Sqlite>` or `sqlx::Pool<Postgres>` in consumer crates.** All
   crates use `zeph_db::DbPool`. Direct sqlx pool type references are prohibited
   outside `zeph-db`.

3. **All SQL strings pass through `sql!()`.** This ensures placeholder compatibility.
   Queries without `sql!()` are linting violations.

4. **Migration parity.** The SQLite and PostgreSQL migration directories must have
   matching file counts and schema-equivalent content.

5. **SQLite remains the default.** PostgreSQL is opt-in. No user action required
   to continue using SQLite after this change.

6. **No `sqlx::Any`.** The `Any` backend is never used. Backend selection is
   compile-time, not runtime.

7. **Amendment [2026-03-28]: PostgreSQL JSONB queries bypass `sql!()`.**
   Queries using PostgreSQL JSONB operators (`?`, `?|`, `?&`) must not pass
   through `sql!()` or `rewrite_placeholders()`. They must use `$N` placeholders
   directly and carry a `// SAFETY: uses PG JSONB operators` annotation. This
   is a hard invariant — violating it produces silently malformed queries.

8. **Amendment [2026-03-28]: `GlobalScope` is `pub(crate)` only.**
   `GlobalScope::new()` cannot be called from consumer crates. Only the root
   binary crate's admin/CLI path may construct a `GlobalScope`. This prevents
   accidental or intentional bypass of agent_id filtering in agent code.

9. **Amendment 2 [2026-03-28]: No backend name in generic types.**
   No struct or type in `zeph-db` or any consumer crate embeds the backend name
   ("Sqlite", "Postgres") as part of a generic concept. Use type parameters
   (`D: DatabaseDriver`) instead. Concrete backend names appear only in:
   (a) the `DatabaseDriver` implementors themselves (`SqliteDriver`, `PostgresDriver`),
   (b) the `Dialect` implementors (`Sqlite`, `Postgres`), and
   (c) backward-compatible type aliases (`SqliteStore = DbStore`, `SqliteVectorStore`).
   New code must use the generic forms.

---

## 13. Agent Boundaries

### Always (without asking)
- Run tests after changes.
- Follow existing code patterns (error handling, naming, doc comments).
- Wrap all SQL strings in `sql!()` macro.
- Use `D: DatabaseDriver` type parameter or `zeph_db::DbPool` alias in consumer crates — never `sqlx::SqlitePool` or `sqlx::PgPool` directly.
- Use `D::Dialect::NOW` (or `ActiveDialect::NOW`) for dialect constants — never `#[cfg]`-gated raw strings in consumer crates.

### Ask First
- Adding the `zeph-db` crate to the workspace.
- Moving migration files from `zeph-memory` to `zeph-db`.
- Renaming `SqliteStore` to `DbStore`.
- Amending the constitution to allow same-layer infrastructure crate imports.

### Never
- Use `sqlx::Any` backend.
- Remove SQLite support or make PostgreSQL the default.
- Introduce `openssl-sys` via `sqlx/tls-native-tls` feature.
- Mix placeholder styles (`?` and `$N`) in the same query string.
- Embed backend names ("Sqlite", "Postgres") in new generic type names (Invariant 9).

---

## 14. Success Criteria

| ID | Metric | Target |
|----|--------|--------|
| SC-001 | All existing unit tests pass with `--features sqlite` | 7042/7042 |
| SC-002 | Build succeeds with `--features postgres` | Clean build |
| SC-003 | PostgreSQL integration tests pass (basic CRUD) | All pass |
| SC-004 | No runtime performance regression on SQLite | < 5% latency change |
| SC-005 | Default build time unchanged | Within 10% of current |
| SC-006 | Migration count parity | SQLite count == PostgreSQL count |

---

## 15. Open Questions

- **[NEEDS CLARIFICATION: pgvector]** Should the PostgreSQL backend support native
  `pgvector` for embedding storage, replacing the Qdrant dependency for PG-only
  deployments? This would add significant scope but simplify the deployment story.

- **[NEEDS CLARIFICATION: concurrent migration]** Should the PostgreSQL migration
  runner use advisory locks (`pg_advisory_lock`) to prevent concurrent migration
  execution from multiple Zeph instances? sqlx may handle this already.

- **[NEEDS CLARIFICATION: schema convergence for scheduler/mcp]** The scheduler
  and MCP crates use inline `CREATE TABLE IF NOT EXISTS` instead of migrations.
  Should Phase 1 consolidate these into the `zeph-db` migration pipeline, or
  leave them as-is and only consolidate in Phase 2?

---

## 16. Integration Tests with testcontainers

### 16.1 Approach

PostgreSQL integration tests use [`testcontainers-rs`](https://github.com/testcontainers/testcontainers-rs)
to spin up a real PostgreSQL instance per test suite. No external Postgres is required —
the container lifecycle is managed by the test runtime.

All PostgreSQL integration tests are gated with `#[ignore]` and run via:

```bash
cargo nextest run --config-file .github/nextest.toml -p zeph-db --features postgres --ignored
```

CI adds a separate job (see Phase 3, step 5) that runs these tests against a service
container or Docker-in-Docker.

### 16.2 Dependencies

Add to `zeph-db/Cargo.toml` under `[dev-dependencies]`:

```toml
[dev-dependencies]
testcontainers = "0.23"
testcontainers-modules = { version = "0.11", features = ["postgres"] }
tokio = { workspace = true, features = ["rt-multi-thread", "macros"] }
```

The `testcontainers-modules` crate provides a pre-built `Postgres` image descriptor
(defaults to `postgres:16-alpine`).

### 16.3 Test Fixture

Define a shared fixture in `crates/zeph-db/tests/common/mod.rs`:

```rust
// crates/zeph-db/tests/common/mod.rs

use testcontainers::{ContainerAsync, ImageExt};
use testcontainers_modules::postgres::Postgres;
use zeph_db::{DbConfig, DbPool};

/// Starts a PostgreSQL container and returns (pool, container).
///
/// The container is kept alive as long as `_container` is in scope.
/// Drop `_container` to stop and remove the container.
pub async fn pg_pool() -> (DbPool, ContainerAsync<Postgres>) {
    let image = Postgres::default()
        .with_tag("16-alpine")
        .with_env_var("POSTGRES_DB", "zeph_test")
        .with_env_var("POSTGRES_USER", "zeph")
        .with_env_var("POSTGRES_PASSWORD", "zeph");

    let container = testcontainers::runners::AsyncRunner::start(image)
        .await
        .expect("failed to start postgres container");

    let host = container.get_host().await.expect("container host");
    let port = container.get_host_port_ipv4(5432).await.expect("container port");

    let url = format!("postgres://zeph:zeph@{host}:{port}/zeph_test");

    let pool = DbConfig {
        backend: "postgres".into(),
        url,
        max_connections: 5,
    }
    .connect()
    .await
    .expect("failed to connect and migrate");

    (pool, container)
}
```

### 16.4 Test Suites

#### Migration smoke test

```rust
// crates/zeph-db/tests/pg_migrations.rs
#[cfg(feature = "postgres")]
mod tests {
    use super::common::pg_pool;

    #[tokio::test]
    #[ignore = "requires docker"]
    async fn all_migrations_apply_cleanly() {
        let (_pool, _container) = pg_pool().await;
        // Pool construction runs migrations; if we reach here, all 49 migrations applied.
    }

    #[tokio::test]
    #[ignore = "requires docker"]
    async fn migrations_are_idempotent() {
        let (pool, _container) = pg_pool().await;
        // Re-running migrations on an already-migrated schema must not fail.
        sqlx::migrate!("./migrations/postgres")
            .run(&pool)
            .await
            .expect("idempotency check failed");
    }
}
```

#### Pool and connection tests

```rust
#[tokio::test]
#[ignore = "requires docker"]
async fn pool_reconnects_after_idle() {
    let (pool, _container) = pg_pool().await;
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    let row: (i64,) = sqlx::query_as("SELECT 1")
        .fetch_one(&pool)
        .await
        .expect("reconnect failed");
    assert_eq!(row.0, 1);
}
```

#### `sql!` placeholder rewriting

```rust
#[cfg(feature = "postgres")]
#[test]
fn sql_macro_rewrites_placeholders() {
    use zeph_db::rewrite_placeholders;

    assert_eq!(
        rewrite_placeholders("SELECT * FROM t WHERE a = ? AND b = ?"),
        "SELECT * FROM t WHERE a = $1 AND b = $2"
    );
    // ? inside string literal must not be rewritten
    assert_eq!(
        rewrite_placeholders("SELECT '?' FROM t WHERE id = ?"),
        "SELECT '?' FROM t WHERE id = $1"
    );
    // Zero placeholders
    assert_eq!(rewrite_placeholders("SELECT 1"), "SELECT 1");
}
```

#### CRUD round-trip per subsystem

For each store migrated to `zeph-db`, add a `#[ignore]`-gated test that:

1. Creates the pool via `pg_pool()`.
2. Inserts a record.
3. Reads it back and asserts equality.
4. Deletes it and asserts absence.

Example for `messages` table (tested from `zeph-memory` integration tests):

```rust
// crates/zeph-memory/tests/pg_store.rs
#[cfg(feature = "postgres")]
mod tests {
    use zeph_memory::SqliteStore; // or DbStore after rename
    // ... use pg_pool from zeph-db test fixture via re-export or inline

    #[tokio::test]
    #[ignore = "requires docker"]
    async fn message_crud_roundtrip() {
        let (pool, _container) = zeph_db::test_utils::pg_pool().await;
        let store = DbStore::from_pool(pool);

        let cid = "test-conv-1";
        store.save_message(cid, "user", "hello", &[]).await.unwrap();

        let messages = store.load_history(cid, 10).await.unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].content, "hello");
    }
}
```

**Note**: `zeph_db::test_utils` is a `#[cfg(test)]` / `#[cfg(feature = "test-utils")]`
module that re-exports `pg_pool()` so consumer crates don't need to add
`testcontainers` as a direct dependency.

#### FTS parity test

```rust
#[tokio::test]
#[ignore = "requires docker"]
async fn fts_returns_matching_messages() {
    let (pool, _container) = pg_pool().await;
    // Insert messages, run FTS query, assert expected rows returned.
    // Does NOT assert identical ranking to SQLite — results are backend-specific.
}
```

#### Transaction isolation test

```rust
#[tokio::test]
#[ignore = "requires docker"]
async fn concurrent_writes_do_not_deadlock() {
    let (pool, _container) = pg_pool().await;
    let pool = std::sync::Arc::new(pool);
    let mut handles = Vec::new();
    for i in 0..10 {
        let p = pool.clone();
        handles.push(tokio::spawn(async move {
            let mut tx = zeph_db::begin_write(&p).await.unwrap();
            sqlx::query("INSERT INTO messages(conversation_id, role, content) VALUES ($1, $2, $3)")
                .bind(format!("conv-{i}"))
                .bind("user")
                .bind(format!("message {i}"))
                .execute(&mut *tx)
                .await
                .unwrap();
            tx.commit().await.unwrap();
        }));
    }
    for h in handles { h.await.unwrap(); }
}
```

### 16.5 `test-utils` Feature

To allow consumer crates to use the `pg_pool()` fixture without depending on
`testcontainers` in production builds:

```toml
# zeph-db/Cargo.toml
[features]
test-utils = ["dep:testcontainers", "dep:testcontainers-modules"]

[dependencies]
testcontainers = { version = "0.23", optional = true }
testcontainers-modules = { version = "0.11", features = ["postgres"], optional = true }
```

```rust
// zeph-db/src/test_utils.rs  (only compiled with test-utils feature)
#[cfg(feature = "test-utils")]
pub mod test_utils {
    pub use crate::tests::common::pg_pool;
}
```

Consumer crates enable it in `[dev-dependencies]` only:

```toml
# crates/zeph-memory/Cargo.toml
[dev-dependencies]
zeph-db = { workspace = true, features = ["postgres", "test-utils"] }
```

### 16.6 CI Integration

Add a GitHub Actions job in `.github/workflows/ci.yml`:

```yaml
test-postgres:
  name: Integration tests (PostgreSQL)
  runs-on: ubuntu-latest
  services:
    postgres:
      image: postgres:16-alpine
      env:
        POSTGRES_DB: zeph_test
        POSTGRES_USER: zeph
        POSTGRES_PASSWORD: zeph
      ports: ["5432:5432"]
      options: >-
        --health-cmd pg_isready
        --health-interval 10s
        --health-timeout 5s
        --health-retries 5
  steps:
    - uses: actions/checkout@v4
    - uses: dtolnay/rust-toolchain@stable
    - run: |
        cargo nextest run \
          --config-file .github/nextest.toml \
          --workspace \
          --features postgres \
          --no-default-features \
          --ignored \
          --test-threads 1
      env:
        # testcontainers uses the service container; override URL for non-testcontainers tests
        DATABASE_URL: postgres://zeph:zeph@localhost:5432/zeph_test
```

**Amendment [2026-03-28]**: The previous note about `--test-threads 1` was
incorrect. `--test-threads 1` (or `-j 1`) controls parallelism *within* a single
test binary, not across binaries. `cargo nextest` runs each test binary as a
separate process. To serialize across binaries and prevent Docker resource
exhaustion, use a nextest profile:

```toml
# .github/nextest.toml
[profile.postgres]
test-threads = 1

[[profile.postgres.overrides]]
filter = 'package(zeph-db) | package(zeph-memory)'
threads-required = 2
```

Run with: `cargo nextest run --profile postgres ...`

The actual risk is not port collision (testcontainers maps random host ports) but
Docker resource exhaustion (CPU, memory, container limit) from multiple containers
starting simultaneously. Recommend running PostgreSQL integration tests in a
separate CI job with the `postgres` nextest profile.

### 16.7 Success Criteria for Tests

| ID | Metric | Target |
|----|--------|--------|
| TC-001 | All migrations apply cleanly on fresh PG instance | Pass |
| TC-002 | Migration re-run is idempotent | Pass |
| TC-003 | `sql!` placeholder rewriting passes all edge cases | Pass |
| TC-004 | CRUD round-trip for messages, graph entities, scheduler jobs | Pass |
| TC-005 | 10 concurrent writes complete without deadlock | Pass |
| TC-006 | FTS returns expected results (content match, not ranking) | Pass |
| TC-007 | CI PostgreSQL job completes in < 5 minutes | Pass |

---

## 17. References

- sqlx `Any` backend docs: https://docs.rs/sqlx/latest/sqlx/any/index.html
- sqlx feature flags: https://docs.rs/sqlx/latest/sqlx/#feature-flags
- PostgreSQL FTS: https://www.postgresql.org/docs/current/textsearch.html
- testcontainers-rs: https://github.com/testcontainers/testcontainers-rs
- testcontainers-modules (postgres): https://github.com/testcontainers/testcontainers-rs/tree/main/testcontainers-modules
- Existing spec `004-memory/spec.md` — SQLite store invariants
- Existing spec `018-scheduler/spec.md` — Scheduler persistence
- Constitution: `.local/specs/constitution.md` — Layer rules

---

## 18. Agent Identity in the Shared Data Model

### 18.1 Problem

With SQLite, exactly one Zeph agent process accesses the database (single-writer
guarantee, single user). With PostgreSQL, multiple agent instances can connect to
the same shared database simultaneously. Rows must be attributable to a specific
agent instance so that:

1. Each agent manages its own conversation history, memory, and state independently.
2. Agents can be isolated (agent A cannot read agent B's private memory).
3. Agents can share subsystems selectively (shared knowledge graph, shared code
   index, private conversations).
4. Migrations remain safe under concurrent startup.

### 18.2 Agent Identity Concept

An **agent identity** is a stable, human-readable string that uniquely identifies
a logical Zeph agent within a shared database. It is distinct from a runtime
process ID.

**Definition**:

| Concept | Type | Source | Purpose |
|---------|------|--------|---------|
| `agent_id` | `Arc<str>` (max 64 chars, `[a-z0-9_-]`) | Config field `[agent] id`, or hostname fallback | Primary isolation key in all DB queries |
| `instance_uuid` | `Uuid` (v7, time-ordered) | Generated at startup, never persisted in config | Fine-grained instance tracking in logs and metrics; NOT used for DB isolation |

**Resolution order** (at bootstrap, before pool construction):

1. If `[agent] id` is set in TOML config, use it verbatim.
2. Else, derive from system hostname: `hostname | tr 'A-Z' 'a-z' | tr -c 'a-z0-9_-' '-' | head -c 64`.
   **Amendment [2026-03-28]**: Note that dots in hostnames (e.g., `host.example.com`)
   are replaced with `-` (e.g., `host-example-com`). If the sanitized result starts
   with `-` (e.g., hostname `.local` becomes `-local`), fall back to `"default"`.
3. Validate against regex `^[a-z0-9][a-z0-9_-]{0,63}$`. Reject and fail startup if invalid.

**Relationship to `conversation_id`**: `conversation_id` identifies a single
conversation _within_ an agent. The hierarchy is:

```
agent_id  (logical agent — "my-bot", "team-shared", "default")
  └── conversation_id  (one of many conversations owned by that agent)
        └── message_id  (message within the conversation)
```

For single-agent SQLite deployments, `agent_id` defaults to `"default"` and is
invisible to the user.

**Newtype wrapper**:

```rust
// zeph-db/src/identity.rs

/// Validated agent identifier. Immutable after construction.
///
/// Format: 1-64 characters, `[a-z0-9][a-z0-9_-]*`.
/// Used as the primary isolation key in all database queries.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AgentId(Arc<str>);

impl AgentId {
    /// The default agent ID for single-agent deployments.
    pub const DEFAULT: &str = "default";

    /// Parse and validate an agent ID string.
    ///
    /// # Errors
    ///
    /// Returns an error if the string is empty, exceeds 64 characters,
    /// or contains characters outside `[a-z0-9_-]`.
    pub fn parse(s: impl Into<String>) -> Result<Self, AgentIdError> {
        let s = s.into();
        if s.is_empty() || s.len() > 64 {
            return Err(AgentIdError::InvalidLength(s.len()));
        }
        let bytes = s.as_bytes();
        if !bytes[0].is_ascii_lowercase() && !bytes[0].is_ascii_digit() {
            return Err(AgentIdError::InvalidStart(s));
        }
        if !s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-') {
            return Err(AgentIdError::InvalidCharacters(s));
        }
        Ok(Self(Arc::from(s)))
    }

    /// Return the default agent ID. Always valid.
    #[must_use]
    pub fn default_id() -> Self {
        Self(Arc::from(Self::DEFAULT))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for AgentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AgentIdError {
    #[error("agent_id length must be 1-64 characters, got {0}")]
    InvalidLength(usize),
    #[error("agent_id must start with [a-z0-9], got: {0}")]
    InvalidStart(String),
    #[error("agent_id contains invalid characters (only [a-z0-9_-] allowed): {0}")]
    InvalidCharacters(String),
}
```

### 18.3 Isolation Model

Two modes govern how queries are scoped:

| Mode | Behavior | Use Case |
|------|----------|----------|
| **Isolated** (default) | Every query includes `WHERE agent_id = ?`. Agent A cannot see agent B's rows. | Conversations, episodic memory, scheduler jobs, session history |
| **Shared** (opt-in per subsystem) | Rows have `agent_id = NULL` (global) or are visible to all agents. Queries omit or ignore the `agent_id` filter. | Knowledge graph, code index metadata, MCP trust scores, plan cache |

**Per-table default isolation mode**:

| Table(s) | Default Mode | Rationale |
|-----------|-------------|-----------|
| `conversations`, `messages`, `summaries`, `mem_scenes`, `mem_scene_members` | **Isolated** | Conversation history is private per agent — this is the core isolation boundary |
| `embeddings_metadata`, `vector_points`, `vector_collections` | **Isolated** | Embeddings reference agent-specific messages |
| `input_history` | **Isolated** | User input is per-agent |
| `tool_overflow` | **Isolated** | Tool output belongs to a specific agent session |
| `session_digest` | **Isolated** | Per-conversation, per-agent |
| `user_corrections` | **Isolated** | User feedback is agent-specific |
| `learned_preferences` | **Isolated** | Learned from agent-specific interactions |
| `acp_sessions`, `acp_session_events` | **Isolated** | ACP sessions are per-agent |
| `experiment_results` | **Isolated** | Experiments are per-agent runs |
| `scheduled_jobs` | **Isolated** | Each agent manages its own schedule |
| `graph_entities`, `graph_edges`, `graph_communities`, `graph_entity_aliases`, `graph_metadata` | **Isolated** | **Amendment [2026-03-28]**: Changed from Shared to Isolated. In shared mode, graph entities extracted from private conversations leak derived information (entity names, edge types, timestamps) to other agents. Shared graph mode should only be used when all agents belong to the same trust domain. A `source_agent_id TEXT` nullable column is added for provenance tracking (distinct from the isolation `agent_id`). |
| `chunk_metadata` (code index) | **Shared** | Code index is read-only, no personal data, same codebase for all agents |
| `skill_usage`, `skill_versions`, `skill_outcomes`, `skill_trust` | **Shared** | Skills are shared infrastructure — trust scores and usage stats benefit all agents |
| `response_cache`, `semantic_response_cache` | **Shared** | Cache hits benefit all agents; duplicate caching wastes space |
| `compression_guidelines`, `compression_failure_pairs` | **Shared** | Learned compression heuristics apply globally |
| `mcp_trust_scores` | **Shared** | Trust in MCP servers is agent-independent |
| `plan_cache` | **Shared** | Cached plans are reusable across agents |
| `task_graphs` | **Isolated** | Task execution belongs to a specific agent session |

**Configurable overrides**: The mode for "Shared" tables can be switched to
"Isolated" via config when strict multi-tenancy is required (e.g., different
customers sharing a database):

```toml
[database]
isolation = "isolated"                              # default
# Override defaults: make plan_cache and code_index agent-private
shared_subsystems = ["code_index", "response_cache", "skills", "mcp_trust", "compression"]
# Omitting "plan_cache" from this list makes it isolated
```

When `isolation = "shared"`, the listed subsystems use `agent_id IS NULL` (global)
for queries. When `isolation = "isolated"`, every subsystem is scoped to the agent
regardless of `shared_subsystems`.

**Amendment [2026-03-28]**: Shared-to-Isolated mode transition requirements:

Switching a subsystem from Shared to Isolated mode creates a data visibility gap:
rows written in Shared mode have `agent_id = NULL`, but Isolated mode filters with
`WHERE agent_id = ?`, which never matches `NULL` (SQL three-valued logic).

Requirements:

1. **Data migration**: Before switching from Shared to Isolated, run:
   ```sql
   UPDATE <table> SET agent_id = 'my-agent' WHERE agent_id IS NULL;
   ```
   This must be documented in the config migration guide and the `--migrate-config`
   output when `shared_subsystems` changes.

2. **Startup check**: At bootstrap, after resolving `AgentScope` per subsystem,
   check if any newly-Isolated subsystem's tables contain `agent_id IS NULL` rows.
   If so, emit a warning:
   ```
   WARN: table 'graph_entities' has rows with agent_id = NULL but subsystem
   is configured as Isolated. These rows are invisible. Run:
     UPDATE graph_entities SET agent_id = '<agent_id>' WHERE agent_id IS NULL;
   ```

3. **Transitional query mode** (optional): During migration, Isolated-mode reads
   can use `WHERE (agent_id = ? OR agent_id IS NULL)` to include legacy NULL rows.
   This is opt-in via `[database] include_shared_rows = true` (default false) and
   should be disabled after migration is complete.

### 18.4 Schema Changes

#### 18.4.1 Column Addition Strategy

Every table gains an `agent_id` column. The column semantics depend on isolation mode:

- **Isolated tables**: `agent_id TEXT NOT NULL` — every row belongs to exactly one agent.
- **Shared tables**: `agent_id TEXT` (nullable) — `NULL` means the row is global/shared.

The migration (numbered `050_agent_identity.sql`) is part of Phase 2 of the DB
abstraction plan. It runs during the same release that introduces `zeph-db`.

#### 18.4.2 SQLite Migration

```sql
-- Migration 050: Agent identity for multi-agent deployments.
--
-- For SQLite (single-agent), all existing rows get agent_id = 'default'.
-- The DEFAULT clause ensures new rows also get 'default' without code changes.
-- ALTER TABLE ADD COLUMN with constant DEFAULT is O(1) in SQLite (no table rewrite).

-- Isolated tables: NOT NULL with default 'default'
ALTER TABLE conversations         ADD COLUMN agent_id TEXT NOT NULL DEFAULT 'default';
ALTER TABLE messages              ADD COLUMN agent_id TEXT NOT NULL DEFAULT 'default';
ALTER TABLE summaries             ADD COLUMN agent_id TEXT NOT NULL DEFAULT 'default';
ALTER TABLE input_history         ADD COLUMN agent_id TEXT NOT NULL DEFAULT 'default';
ALTER TABLE tool_overflow         ADD COLUMN agent_id TEXT NOT NULL DEFAULT 'default';
ALTER TABLE session_digest        ADD COLUMN agent_id TEXT NOT NULL DEFAULT 'default';
ALTER TABLE user_corrections      ADD COLUMN agent_id TEXT NOT NULL DEFAULT 'default';
ALTER TABLE learned_preferences   ADD COLUMN agent_id TEXT NOT NULL DEFAULT 'default';
ALTER TABLE acp_sessions          ADD COLUMN agent_id TEXT NOT NULL DEFAULT 'default';
ALTER TABLE experiment_results    ADD COLUMN agent_id TEXT NOT NULL DEFAULT 'default';
ALTER TABLE task_graphs           ADD COLUMN agent_id TEXT NOT NULL DEFAULT 'default';
ALTER TABLE mem_scenes            ADD COLUMN agent_id TEXT NOT NULL DEFAULT 'default';
ALTER TABLE embeddings_metadata   ADD COLUMN agent_id TEXT NOT NULL DEFAULT 'default';

-- Shared tables: nullable, NULL = global
ALTER TABLE graph_entities        ADD COLUMN agent_id TEXT DEFAULT NULL;
ALTER TABLE graph_edges           ADD COLUMN agent_id TEXT DEFAULT NULL;
ALTER TABLE graph_communities     ADD COLUMN agent_id TEXT DEFAULT NULL;
ALTER TABLE graph_metadata        ADD COLUMN agent_id TEXT DEFAULT NULL;
ALTER TABLE chunk_metadata        ADD COLUMN agent_id TEXT DEFAULT NULL;
ALTER TABLE skill_usage           ADD COLUMN agent_id TEXT DEFAULT NULL;
ALTER TABLE skill_versions        ADD COLUMN agent_id TEXT DEFAULT NULL;
ALTER TABLE skill_outcomes        ADD COLUMN agent_id TEXT DEFAULT NULL;
ALTER TABLE skill_trust           ADD COLUMN agent_id TEXT DEFAULT NULL;
ALTER TABLE response_cache        ADD COLUMN agent_id TEXT DEFAULT NULL;
ALTER TABLE compression_guidelines ADD COLUMN agent_id TEXT DEFAULT NULL;
ALTER TABLE plan_cache            ADD COLUMN agent_id TEXT DEFAULT NULL;
ALTER TABLE vector_collections    ADD COLUMN agent_id TEXT DEFAULT NULL;
ALTER TABLE vector_points         ADD COLUMN agent_id TEXT DEFAULT NULL;

-- Covering indexes for agent-scoped queries on high-traffic isolated tables.
-- Composite indexes with agent_id as prefix for efficient range scans.
CREATE INDEX IF NOT EXISTS idx_messages_agent_conv
    ON messages(agent_id, conversation_id, id)
    WHERE deleted_at IS NULL;

CREATE INDEX IF NOT EXISTS idx_conversations_agent
    ON conversations(agent_id, id);

CREATE INDEX IF NOT EXISTS idx_summaries_agent_conv
    ON summaries(agent_id, conversation_id);

CREATE INDEX IF NOT EXISTS idx_session_digest_agent
    ON session_digest(agent_id, conversation_id);

CREATE INDEX IF NOT EXISTS idx_task_graphs_agent
    ON task_graphs(agent_id, status);

CREATE INDEX IF NOT EXISTS idx_mem_scenes_agent
    ON mem_scenes(agent_id);

CREATE INDEX IF NOT EXISTS idx_acp_sessions_agent
    ON acp_sessions(agent_id);

CREATE INDEX IF NOT EXISTS idx_experiment_results_agent
    ON experiment_results(agent_id, session_id);

-- **Amendment [2026-03-28]**: Additional composite indexes for tables identified
-- as missing coverage in the performance review.

-- embeddings_metadata: queried by (agent_id, conversation_id) in EmbeddingStore.
CREATE INDEX IF NOT EXISTS idx_embeddings_metadata_agent_conv
    ON embeddings_metadata(agent_id, conversation_id);

-- response_cache: queried by (agent_id, cache_key) when isolation is overridden.
CREATE INDEX IF NOT EXISTS idx_response_cache_agent_key
    ON response_cache(agent_id, cache_key);

-- **Amendment [2026-03-28]**: source_agent_id for graph provenance tracking.
-- Distinct from agent_id (which controls isolation). Records which agent
-- originally created the entity/edge, even in shared mode.
ALTER TABLE graph_entities ADD COLUMN source_agent_id TEXT DEFAULT NULL;
ALTER TABLE graph_edges    ADD COLUMN source_agent_id TEXT DEFAULT NULL;
```

#### 18.4.3 PostgreSQL Migration

```sql
-- Migration 050: Agent identity for multi-agent deployments (PostgreSQL variant).

-- Isolated tables
ALTER TABLE conversations         ADD COLUMN agent_id TEXT NOT NULL DEFAULT 'default';
ALTER TABLE messages              ADD COLUMN agent_id TEXT NOT NULL DEFAULT 'default';
ALTER TABLE summaries             ADD COLUMN agent_id TEXT NOT NULL DEFAULT 'default';
ALTER TABLE input_history         ADD COLUMN agent_id TEXT NOT NULL DEFAULT 'default';
ALTER TABLE tool_overflow         ADD COLUMN agent_id TEXT NOT NULL DEFAULT 'default';
ALTER TABLE session_digest        ADD COLUMN agent_id TEXT NOT NULL DEFAULT 'default';
ALTER TABLE user_corrections      ADD COLUMN agent_id TEXT NOT NULL DEFAULT 'default';
ALTER TABLE learned_preferences   ADD COLUMN agent_id TEXT NOT NULL DEFAULT 'default';
ALTER TABLE acp_sessions          ADD COLUMN agent_id TEXT NOT NULL DEFAULT 'default';
ALTER TABLE experiment_results    ADD COLUMN agent_id TEXT NOT NULL DEFAULT 'default';
ALTER TABLE task_graphs           ADD COLUMN agent_id TEXT NOT NULL DEFAULT 'default';
ALTER TABLE mem_scenes            ADD COLUMN agent_id TEXT NOT NULL DEFAULT 'default';
ALTER TABLE embeddings_metadata   ADD COLUMN agent_id TEXT NOT NULL DEFAULT 'default';

-- Shared tables
ALTER TABLE graph_entities        ADD COLUMN agent_id TEXT DEFAULT NULL;
ALTER TABLE graph_edges           ADD COLUMN agent_id TEXT DEFAULT NULL;
ALTER TABLE graph_communities     ADD COLUMN agent_id TEXT DEFAULT NULL;
ALTER TABLE graph_metadata        ADD COLUMN agent_id TEXT DEFAULT NULL;
ALTER TABLE chunk_metadata        ADD COLUMN agent_id TEXT DEFAULT NULL;
ALTER TABLE skill_usage           ADD COLUMN agent_id TEXT DEFAULT NULL;
ALTER TABLE skill_versions        ADD COLUMN agent_id TEXT DEFAULT NULL;
ALTER TABLE skill_outcomes        ADD COLUMN agent_id TEXT DEFAULT NULL;
ALTER TABLE skill_trust           ADD COLUMN agent_id TEXT DEFAULT NULL;
ALTER TABLE response_cache        ADD COLUMN agent_id TEXT DEFAULT NULL;
ALTER TABLE compression_guidelines ADD COLUMN agent_id TEXT DEFAULT NULL;
ALTER TABLE plan_cache            ADD COLUMN agent_id TEXT DEFAULT NULL;
ALTER TABLE vector_collections    ADD COLUMN agent_id TEXT DEFAULT NULL;
ALTER TABLE vector_points         ADD COLUMN agent_id TEXT DEFAULT NULL;

-- **Amendment [2026-03-28]**: All indexes use regular CREATE INDEX (not CONCURRENTLY).
-- CREATE INDEX CONCURRENTLY cannot run inside a transaction block, and sqlx::migrate!
-- runs each migration inside a transaction. Regular CREATE INDEX takes a brief
-- ACCESS EXCLUSIVE lock but is acceptable for a one-time migration.
-- For very large tables in production, concurrent index creation can be done
-- manually out-of-band after the migration.

CREATE INDEX IF NOT EXISTS idx_messages_agent_conv
    ON messages(agent_id, conversation_id, id)
    WHERE deleted_at IS NULL;

CREATE INDEX IF NOT EXISTS idx_conversations_agent
    ON conversations(agent_id, id);

CREATE INDEX IF NOT EXISTS idx_summaries_agent_conv
    ON summaries(agent_id, conversation_id);

CREATE INDEX IF NOT EXISTS idx_session_digest_agent
    ON session_digest(agent_id, conversation_id);

CREATE INDEX IF NOT EXISTS idx_task_graphs_agent
    ON task_graphs(agent_id, status);

CREATE INDEX IF NOT EXISTS idx_mem_scenes_agent
    ON mem_scenes(agent_id);

CREATE INDEX IF NOT EXISTS idx_acp_sessions_agent
    ON acp_sessions(agent_id);

CREATE INDEX IF NOT EXISTS idx_experiment_results_agent
    ON experiment_results(agent_id, session_id);

-- **Amendment [2026-03-28]**: Additional composite indexes (see perf review F5, F6).
CREATE INDEX IF NOT EXISTS idx_embeddings_metadata_agent_conv
    ON embeddings_metadata(agent_id, conversation_id);

CREATE INDEX IF NOT EXISTS idx_response_cache_agent_key
    ON response_cache(agent_id, cache_key);

-- **Amendment [2026-03-28]**: source_agent_id for graph provenance tracking.
ALTER TABLE graph_entities ADD COLUMN source_agent_id TEXT DEFAULT NULL;
ALTER TABLE graph_edges    ADD COLUMN source_agent_id TEXT DEFAULT NULL;
```

#### 18.4.4 Primary Key Considerations

`agent_id` is **not** added to existing primary keys. Existing PKs (`id INTEGER
PRIMARY KEY AUTOINCREMENT` for most tables) remain the physical row identifier.
`agent_id` is enforced via:

1. Composite indexes for query performance (see above).
2. Application-level enforcement via `AgentScope` (see 18.5).
3. For tables with natural UNIQUE constraints that should be per-agent (e.g.,
   `skill_usage.skill_name`, `scheduled_jobs.name`), add a new UNIQUE constraint
   on `(agent_id, skill_name)` and drop the old one — but only when the subsystem
   is in "isolated" mode. When shared, the existing UNIQUE constraint on the
   natural key remains correct.

Tables needing UNIQUE constraint updates in isolated mode:

| Table | Current UNIQUE | New UNIQUE (isolated) | Shared mode |
|-------|---------------|----------------------|-------------|
| `skill_usage` | `(skill_name)` | `(agent_id, skill_name)` | Keep `(skill_name)` |
| `scheduled_jobs`* | `(name)` | `(agent_id, name)` | N/A (always isolated) |
| `graph_entities` | `(name, entity_type)` | Keep (shared by default) | Keep |
| `response_cache` | `(cache_key)` | Keep (shared by default) | Keep |
| `plan_cache` | `(goal_hash)` | Keep (shared by default) | Keep |

\* `scheduled_jobs` is managed by `zeph-scheduler` with inline DDL; the UNIQUE
constraint update happens in the scheduler's migration path.

### 18.5 Query Layer: `AgentScope`

Rather than passing `agent_id` as a parameter to every store method (error-prone,
verbose), introduce an `AgentScope` wrapper that pre-binds the agent identity and
isolation mode. Every store receives an `AgentScope` at construction time.

```rust
// zeph-db/src/scope.rs

use crate::{AgentId, DbPool};
use std::sync::Arc;

/// Isolation mode for a database subsystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IsolationMode {
    /// Queries are scoped to a single agent_id. Other agents' rows are invisible.
    Isolated,
    /// Queries see all rows regardless of agent_id. Writes use agent_id = NULL.
    Shared,
}

/// Pre-bound database scope carrying pool + agent identity + isolation mode.
///
/// Constructed once at startup and cloned into each store. The `agent_id` and
/// `isolation` are immutable for the lifetime of the process.
#[derive(Debug, Clone)]
pub struct AgentScope {
    pool: DbPool,
    agent_id: AgentId,
    isolation: IsolationMode,
}

impl AgentScope {
    #[must_use]
    pub fn new(pool: DbPool, agent_id: AgentId, isolation: IsolationMode) -> Self {
        Self { pool, agent_id, isolation }
    }

    /// **Amendment [2026-03-28]**: `pool()` is `#[doc(hidden)]` with a
    /// deprecation note. Exposing the raw pool allows any store to bypass
    /// agent_id filtering by constructing a `GlobalScope`. Prefer using
    /// `AgentScope` query methods or passing `&AgentScope` to query helpers.
    #[doc(hidden)]
    #[deprecated(note = "direct pool access bypasses agent_id filtering; use AgentScope query methods")]
    #[must_use]
    pub fn pool(&self) -> &DbPool {
        &self.pool
    }

    #[must_use]
    pub fn agent_id(&self) -> &AgentId {
        &self.agent_id
    }

    #[must_use]
    pub fn isolation(&self) -> IsolationMode {
        self.isolation
    }

    /// Return the agent_id string to bind in isolated queries.
    ///
    /// Returns `Some(agent_id)` in Isolated mode, `None` in Shared mode.
    #[must_use]
    pub fn filter_value(&self) -> Option<&str> {
        match self.isolation {
            IsolationMode::Isolated => Some(self.agent_id.as_str()),
            IsolationMode::Shared => None,
        }
    }

    /// Return the agent_id to write on new rows.
    ///
    /// Isolated mode: the agent's ID string.
    /// Shared mode: None (NULL in the database).
    #[must_use]
    pub fn write_value(&self) -> Option<&str> {
        self.filter_value()
    }
}

/// Global scope for administrative operations (export, migration, cross-agent queries).
///
/// Bypasses agent_id filtering. Constructed explicitly by admin CLI commands,
/// never by the normal agent loop.
///
/// **Amendment [2026-03-28]**: `GlobalScope::new()` is `pub(crate)` and only
/// accessible from the root binary crate's admin/CLI path. This prevents
/// accidental construction in agent code. A `tracing::warn!` is emitted on
/// construction for audit purposes.
#[derive(Debug, Clone)]
pub struct GlobalScope {
    pool: DbPool,
}

impl GlobalScope {
    /// Construct a GlobalScope for admin operations.
    ///
    /// # Restriction
    ///
    /// This constructor is `pub(crate)` — only the root binary crate (or
    /// `zeph-db` internals) can create a `GlobalScope`. Agent code in
    /// consumer crates cannot construct this type.
    #[must_use]
    pub(crate) fn new(pool: DbPool) -> Self {
        tracing::warn!("GlobalScope constructed — bypasses all agent_id filtering");
        Self { pool }
    }

    #[must_use]
    pub fn pool(&self) -> &DbPool {
        &self.pool
    }
}
```

**Store construction changes**:

```rust
// Before (current):
pub struct SqliteStore {
    pool: SqlitePool,
}

impl SqliteStore {
    pub async fn new(path: &str) -> Result<Self, MemoryError> { ... }
}

// After:
pub struct DbStore {
    scope: AgentScope,
}

impl DbStore {
    pub fn new(scope: AgentScope) -> Self {
        Self { scope }
    }
}
```

**Query pattern — isolated table**:

```rust
// Before:
pub async fn load_history(
    &self,
    conversation_id: ConversationId,
    limit: i64,
) -> Result<Vec<MessageRow>, MemoryError> {
    let rows = sqlx::query_as(
        "SELECT ... FROM messages WHERE conversation_id = ? AND deleted_at IS NULL \
         ORDER BY id DESC LIMIT ?"
    )
    .bind(conversation_id)
    .bind(limit)
    .fetch_all(&self.pool)
    .await?;
    Ok(rows)
}

// After:
pub async fn load_history(
    &self,
    conversation_id: ConversationId,
    limit: i64,
) -> Result<Vec<MessageRow>, MemoryError> {
    let rows = sqlx::query_as(sql!(
        "SELECT ... FROM messages \
         WHERE conversation_id = ? AND agent_id = ? AND deleted_at IS NULL \
         ORDER BY id DESC LIMIT ?"
    ))
    .bind(conversation_id)
    .bind(self.scope.agent_id().as_str())
    .bind(limit)
    .fetch_all(self.scope.pool())
    .await?;
    Ok(rows)
}
```

**Query pattern — shared table (knowledge graph)**:

```rust
impl GraphStore {
    pub async fn find_entity(&self, name: &str) -> Result<Option<GraphEntity>, MemoryError> {
        // Shared mode: no agent_id filter.
        // Isolated mode (overridden): filter by agent_id.
        let query = match self.scope.filter_value() {
            Some(aid) => {
                sqlx::query_as(sql!(
                    "SELECT * FROM graph_entities WHERE name = ? AND agent_id = ?"
                ))
                .bind(name)
                .bind(aid)
                .fetch_optional(self.scope.pool())
                .await?
            }
            None => {
                sqlx::query_as(sql!(
                    "SELECT * FROM graph_entities WHERE name = ?"
                ))
                .bind(name)
                .fetch_optional(self.scope.pool())
                .await?
            }
        };
        Ok(query)
    }
}
```

To reduce this branching, provide a helper on `AgentScope`:

```rust
impl AgentScope {
    /// Append `AND agent_id = ?` to a query when in isolated mode.
    /// Returns the SQL suffix and an optional bind value.
    pub fn agent_filter_clause(&self) -> (&'static str, Option<&str>) {
        match self.isolation {
            IsolationMode::Isolated => (" AND agent_id = ?", Some(self.agent_id.as_str())),
            IsolationMode::Shared => ("", None),
        }
    }
}
```

Usage with the helper:

```rust
pub async fn find_entity(&self, name: &str) -> Result<Option<GraphEntity>, MemoryError> {
    let (filter, bind_val) = self.scope.agent_filter_clause();
    let sql = format!("SELECT * FROM graph_entities WHERE name = ?{filter}");
    let mut q = sqlx::query_as(&sql!(&sql)).bind(name);
    if let Some(aid) = bind_val {
        q = q.bind(aid);
    }
    Ok(q.fetch_optional(self.scope.pool()).await?)
}
```

**Note**: The `format!` + conditional bind approach introduces a minor runtime
cost (string allocation) but keeps the code DRY. For hot-path queries, use
`LazyLock` with pre-built variants for both modes.

### 18.6 Config Design

Extend `AgentConfig` in `zeph-config/src/agent.rs`:

```rust
fn default_agent_id() -> String {
    hostname::get()
        .ok()
        .and_then(|h| h.to_str().map(str::to_owned))
        .map(|h| {
            h.to_lowercase()
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
                .take(64)
                .collect()
        })
        .unwrap_or_else(|| "default".to_string())
}

#[derive(Debug, Deserialize, Serialize)]
pub struct AgentConfig {
    pub name: String,
    /// Stable agent identifier used as the DB isolation key in multi-agent deployments.
    /// Defaults to the system hostname (lowercased, sanitized).
    /// For single-agent SQLite: "default" is used implicitly.
    #[serde(default = "default_agent_id")]
    pub id: String,
    // ... existing fields unchanged ...
}
```

**TOML surface**:

```toml
[agent]
name = "Zeph"
id = "my-agent"           # stable identifier, used as agent_id in DB

[database]
# Isolation mode: "isolated" (default) | "shared"
# "isolated": every subsystem is scoped to agent.id
# "shared": subsystems listed in shared_subsystems see global rows
isolation = "isolated"
# Subsystems that operate in shared mode when isolation = "shared".
# Ignored when isolation = "isolated".
# Valid values: "graph", "code_index", "skills", "response_cache",
#               "mcp_trust", "compression", "plan_cache"
shared_subsystems = ["graph", "code_index", "skills", "response_cache",
                     "mcp_trust", "compression", "plan_cache"]
```

**Config type**:

```rust
// zeph-config/src/memory.rs (additions)

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum IsolationMode {
    #[default]
    Isolated,
    Shared,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SharedSubsystem {
    Graph,
    CodeIndex,
    Skills,
    ResponseCache,
    McpTrust,
    Compression,
    PlanCache,
}
```

**Scope construction at startup** (in `zeph-core` bootstrap):

```rust
// Pseudocode — zeph-core/src/bootstrap.rs

let agent_id = AgentId::parse(&config.agent.id)?;
let pool = DbConfig::from(&config.memory).connect().await?;

// Determine isolation for each subsystem
let is_shared_mode = config.database.isolation == IsolationMode::Shared;

let make_scope = |subsystem: SharedSubsystem| -> AgentScope {
    let isolation = if is_shared_mode
        && config.database.shared_subsystems.contains(&subsystem)
    {
        zeph_db::IsolationMode::Shared
    } else {
        zeph_db::IsolationMode::Isolated
    };
    AgentScope::new(pool.clone(), agent_id.clone(), isolation)
};

// Conversations, messages — always isolated
let memory_scope = AgentScope::new(pool.clone(), agent_id.clone(), zeph_db::IsolationMode::Isolated);
// Knowledge graph — configurable
let graph_scope = make_scope(SharedSubsystem::Graph);
// Scheduler — always isolated
let scheduler_scope = AgentScope::new(pool.clone(), agent_id.clone(), zeph_db::IsolationMode::Isolated);
```

### 18.7 Concurrent Migration Safety

#### 18.7.1 SQLite

No concern. SQLite's single-writer lock serializes everything. Only one process
can write at a time, and the `busy_timeout` PRAGMA handles contention.

#### 18.7.2 PostgreSQL: `sqlx::migrate!` and Advisory Locks

`sqlx::migrate!().run(pool)` on PostgreSQL **already uses advisory locks**
internally. Specifically, sqlx acquires `pg_advisory_lock(hash)` before checking
the `_sqlx_migrations` table and running pending migrations. This means:

- Multiple Zeph instances starting simultaneously against the same PostgreSQL
  database will serialize their migration runs automatically.
- The first instance to acquire the lock runs all pending migrations.
- Subsequent instances wait for the lock, then find all migrations already applied,
  and proceed without running anything.

**No additional locking mechanism is needed.** This resolves the open question
from section 15.

To confirm, the relevant sqlx source (as of 0.8.x):

```rust
// sqlx-core/src/migrate/migrate.rs (simplified)
async fn run(&self, pool: &PgPool) -> Result<()> {
    let lock_id = ... ; // hash of migration source path
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(lock_id)
        .execute(pool)
        .await?;
    // ... run pending migrations ...
    sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(lock_id)
        .execute(pool)
        .await?;
}
```

**Risk**: If a process crashes while holding the advisory lock (between `lock`
and `unlock`), the lock is released automatically when the PostgreSQL session
ends (advisory locks are session-scoped). No manual cleanup is needed.

### 18.8 Impact on Existing SQLite Deployment

| Aspect | Impact |
|--------|--------|
| **Existing rows** | `agent_id` column added with `DEFAULT 'default'`. All existing rows get `agent_id = 'default'` at O(1) cost (SQLite stores the default in the schema, not per-row). |
| **New rows** | If `[agent] id` is not set in config, `agent_id` resolves to `"default"` (or hostname). The `DEFAULT 'default'` clause in DDL serves as a safety net for raw SQL inserts. |
| **Query performance** | The new `WHERE agent_id = 'default'` clause adds a constant-time comparison. The composite indexes ensure no scan regression. |
| **User action required** | None. Existing config files without `[agent] id` or `[database] isolation` work unchanged. |
| **Behavioral change** | Zero. Single-agent SQLite with `agent_id = 'default'` behaves identically to the current agent-unaware queries. |
| **Config migration** | `--migrate-config` adds `id = "default"` under `[agent]` and `isolation = "isolated"` under `[database]` if absent. Non-breaking. |
| **Database file size** | Negligible increase. `agent_id TEXT` column with constant `'default'` value is stored once in the schema header, not per-row (SQLite optimization for constant-default columns added via `ALTER TABLE ADD COLUMN`). Indexes add ~10-20% overhead on indexed tables. |

### 18.9 Risks

#### 18.9.1 Query Verbosity

**Risk**: Every `WHERE` clause gains `AND agent_id = ?`, increasing query
complexity and maintenance burden.

**Mitigation**: The `AgentScope::agent_filter_clause()` helper centralizes the
pattern. For isolated-only tables (conversations, messages), `agent_id = ?` is
always present — no conditional logic needed. The `sql!` macro already handles
placeholder rewriting, so `agent_id` is just another bind parameter.

#### 18.9.2 Index Coverage

**Risk**: Queries that previously used single-column indexes now need composite
indexes with `agent_id` prefix. Missing indexes cause sequential scans.

**Mitigation**: The migration (18.4) creates composite indexes for all
high-traffic query patterns. The `agent_id` prefix is chosen because it has low
cardinality (few distinct values) and PostgreSQL's query planner handles it well
with `Index Only Scan` on `(agent_id, conversation_id, id)`.

**Trade-off**: Index overhead is paid on every write (INSERT, UPDATE, DELETE).
For SQLite single-agent deployments, the composite indexes are redundant with the
existing single-column indexes. Acceptable overhead (~10-20% on indexed tables).

#### 18.9.3 Forgotten agent_id Filter (Data Leakage)

**Risk**: A query that omits the `agent_id` filter returns rows from all agents,
leaking private data across tenants.

**Mitigations (defense in depth)**:

1. **Type-system enforcement**: Stores receive `AgentScope`, not raw `DbPool`.
   The `AgentScope` API makes agent-scoped queries the path of least resistance.
   Accessing the raw pool requires `.pool()`, which is an explicit opt-out.

2. **Code review convention**: All new queries must go through `AgentScope`.
   Direct pool access is reserved for `GlobalScope` (admin operations).

3. **Clippy lint (future)**: A custom clippy lint or `#[deny(direct_pool_access)]`
   attribute macro that flags `.pool()` usage outside whitelisted modules.

4. **Integration test**: Add a test that introspects all SQL query strings in the
   codebase (via a build script or grep) and asserts that every query touching an
   isolated table contains `agent_id`.

5. **PostgreSQL Row-Level Security (future, optional)**: For high-security deployments,
   enable RLS policies on isolated tables that enforce `agent_id = current_setting('app.agent_id')`.
   This is a server-side safety net independent of application logic.

#### 18.9.4 Cross-Agent Operations

**Risk**: Admin tools (data export, migration, global cleanup, analytics) need
to query across all agents.

**Mitigation**: `GlobalScope` type (see 18.5) provides unfiltered pool access.
It is constructed explicitly in admin CLI commands (`zeph db export --all-agents`,
`zeph db stats`), never in the agent loop. The type distinction (`GlobalScope` vs
`AgentScope`) prevents accidental global queries in agent code.

#### 18.9.5 Agent ID Collisions

**Risk**: Two users independently choose the same `agent_id` (e.g., both use
hostname-derived IDs on identically-named hosts) and collide in a shared database.

**Mitigation**: Document that `agent_id` must be unique per logical agent in a
shared database. The `--init` wizard prompts for a unique ID. For automated
deployments, generate IDs from a namespace (e.g., `team-${KUBERNETES_POD_NAME}`).
No runtime enforcement of uniqueness — this is an operational concern, not a
database constraint.

#### 18.9.6 Migration Ordering with Phase 1/2 Split

**Risk**: Migration 050 depends on tables created by the original 49 migrations.
If the DB abstraction Phase 1 (moving migrations to `zeph-db`) and Phase 2
(PostgreSQL + agent identity) are separate releases, the migration numbering must
be coordinated.

**Mitigation**: Agent identity migration (050) ships as part of Phase 2, after
all 49 migrations are successfully ported to both backends. The migration number
is reserved in Phase 1 (an empty `050_reserved_agent_identity.sql` placeholder)
to prevent number conflicts.

### 18.10 Key Invariants

1. **`agent_id` is immutable for the lifetime of a process.** Once resolved at
   startup, it never changes. Hot-reloading config does not alter `agent_id`.

2. **Isolated tables always have `agent_id NOT NULL`.** No row in an isolated
   table can have `agent_id = NULL`. The `NOT NULL DEFAULT 'default'` DDL
   constraint enforces this at the database level.

3. **Shared tables use `NULL` for global rows.** Agent-specific rows in shared
   tables (when a subsystem is overridden to isolated) use the agent's ID.
   Global rows use `NULL`.

4. **`AgentScope` is the sole gateway to the database in agent code.** No store
   in the agent loop may hold a raw `DbPool` reference. Only `GlobalScope` in
   admin commands may bypass agent filtering.

5. **SQLite `'default'` is transparent.** A single-agent SQLite deployment with
   `agent_id = 'default'` is indistinguishable from the pre-agent-identity schema
   in terms of query results and performance.

6. **The conversations → messages hierarchy respects agent_id transitively.**
   If `conversations.agent_id = 'X'`, all messages in that conversation also have
   `agent_id = 'X'`. Application code enforces this; no cross-agent foreign key
   constraint exists (SQLite does not support CHECK constraints referencing other
   tables).

---

## 19. Amendment Log

All amendments dated 2026-03-28. Triggered by three independent reviews:
critic-review-1.md, perf-review-1.md, security-review-1.md.

### CRITICAL / HIGH

| # | Section(s) | Review | Change |
|---|-----------|--------|--------|
| 1 | 4.5, 11.5, 12 | C1 (critic) | Removed false claim that PG does not use `?` for other purposes. Added JSONB operator bypass convention: queries using `?`/`?|`/`?&` must not pass through `sql!()`, must use `$N` directly with safety annotation. Added Key Invariant #7. |
| 2 | 18.3, 18.5 | C2 (critic) | Added shared-to-isolated transition requirements: data migration SQL, startup warning for NULL rows, optional transitional `WHERE (agent_id = ? OR agent_id IS NULL)` query mode. |
| 3 | 18.4.3 | S5 (critic) | Replaced all `CREATE INDEX CONCURRENTLY` with regular `CREATE INDEX` in migration DDL. Added note that concurrent index creation requires manual out-of-band execution. |
| 4 | 4.6, 6.1 | F1 (perf) | Added `write_pool_size` to `DbConfig` (default 1, SQLite only) to prevent `SQLITE_BUSY` stalls from competing writers in a unified pool. |
| 5 | 4.7, 11.2 | F8 (perf) | Mandated `SELECT ... FOR UPDATE` on PostgreSQL for skill trust score updates (the two `BEGIN IMMEDIATE` locations). Changed from audit item to required Phase 2 implementation step. |
| 6 | 4.6, 6.1, 6.2 | F-01 (security) | Added `redact_url()` helper requirement, `DbError::Connection` stores redacted URL only, `ZEPH_DATABASE_URL` as canonical vault key, startup warning for inline credentials, `RedactFilter` regex extension. |
| 7 | 18.5, 12 | S3 (critic), F-02 (security) | `GlobalScope::new()` changed to `pub(crate)`. `AgentScope::pool()` marked `#[doc(hidden)]` + `#[deprecated]`. Added Key Invariant #8. Added `tracing::warn!` on `GlobalScope` construction. |

### MEDIUM

| # | Section(s) | Review | Change |
|---|-----------|--------|--------|
| 8 | 4.2, 7.2, 7.4 | S1 (critic) | Removed `sqlite` from `zeph-db` default features. Both backends are non-default. Root `Cargo.toml` default explicitly includes `zeph-db/sqlite`. Documented that `--all-features` is not supported. |
| 9 | 4.5, 7.2 | S2 (critic), F3 (perf) | Fixed query static pattern: SQLite uses `&'static str` constants, PostgreSQL uses `LazyLock<String>`. Removed incorrect "optimizer eliminates LazyLock" claim. Removed `sqlx/macros` from `zeph-db` features. |
| 10 | 18.3 | S4 (critic) | Changed default for graph tables from Shared to Isolated. Added `source_agent_id` column for provenance tracking. Documented privacy trade-off. |
| 11 | 4.4 | M3 (critic) | Removed `Dialect::bool_val()` — sqlx `Encode` handles bool mapping automatically. |
| 12 | 4.6 | F2 (perf) | Added `acquire_timeout = 30s` to `PgPoolOptions` to prevent silent task deadlock on pool saturation. |
| 13 | 18.4.2, 18.4.3 | F5, F6 (perf) | Added composite indexes for `embeddings_metadata(agent_id, conversation_id)` and `response_cache(agent_id, cache_key)` to both SQLite and PostgreSQL migration DDL. |
| 14 | 16.6 | S6 (critic) | Corrected testcontainers CI config: replaced `--test-threads 1` claim with nextest profile-level serialization. Added `.github/nextest.toml` example for `postgres` profile. |

### LOW (notes)

| # | Section(s) | Review | Change |
|---|-----------|--------|--------|
| 15 | 18.2 | M1 (critic) | Documented that dots in hostnames are replaced with `-`; fall back to `"default"` if sanitized result starts with `-`. |
| 16 | 11.4 | M2 (critic) | Strengthened migration parity: compare `information_schema.columns` (PG) vs `pragma_table_info` (SQLite) as CI step, not just file counts. |
| 17 | 11.5 | F-03 (security) | Documented that `rewrite_placeholders()` does not handle dollar-quoted strings or SQL comments; banned those patterns in shared queries. |

### Amendment 2: Generics and Traits Redesign [2026-03-28]

Triggered by architect review. Replaces `#[cfg]`-gated type aliases and `Dialect`
struct with proper Rust generics and traits.

| # | Section(s) | Change |
|---|-----------|--------|
| 18 | 3.2 | Renamed approach from "Dialect Abstraction with Feature Flags" to "Generics + Traits with Feature-Flag Selection". Updated description to reference `DatabaseDriver` trait. |
| 19 | 4.1 | Updated crate file layout: added `driver.rs`, `driver/sqlite.rs`, `driver/postgres.rs`, `bounds.rs`. |
| 20 | 4.3 (rewritten) | `Dialect` is now a trait (not a struct). `Sqlite` and `Postgres` are zero-sized marker types implementing `Dialect`. Associated constants: `NOW`, `AUTO_PK`, `INSERT_IGNORE`, `CONFLICT_NOTHING`. Method: `ilike()`. |
| 21 | 4.4 (rewritten) | Introduced `DatabaseDriver` trait unifying `type Database: sqlx::Database`, `type Dialect: Dialect`, `connect()`, `run_migrations()`, `begin()`, `begin_write()`. |
| 22 | 4.4.1 | `SqliteDriver` implements `DatabaseDriver` with `Database = sqlx::Sqlite`, `Dialect = Sqlite`. Connection logic moved from `DbConfig::connect_sqlite()`. |
| 23 | 4.4.2 | `PostgresDriver` implements `DatabaseDriver` with `Database = sqlx::Postgres`, `Dialect = Postgres`. Connection logic moved from `DbConfig::connect_postgres()`. |
| 24 | 4.4.3 | `#[cfg]`-gated type aliases (`DbPool`, `DbRow`, `DbTransaction`, `ActiveDriver`, `ActiveDialect`) retained as convenience shortcuts derived from `ActiveDriver` associated types. |
| 25 | 4.4.4 | Generic store pattern: `Store<D: DatabaseDriver>` with `PhantomData<D>`. `DbStore = Store<ActiveDriver>`. Required sqlx trait bounds documented. `FullDriver` blanket super-trait for bound reduction. |
| 26 | 4.4.5 | `SqliteVectorStore` → `DbVectorStore<D: DatabaseDriver>`. Backward-compatible alias retained. |
| 27 | 4.6 | `DbConfig::connect()` now delegates to `ActiveDriver::connect()` + `ActiveDriver::run_migrations()`. Backend-specific `connect_sqlite()`/`connect_postgres()` methods removed from `DbConfig`. |
| 28 | 4.7 | `begin()`/`begin_write()` free functions delegate to `ActiveDriver::begin()`/`ActiveDriver::begin_write()`. Generic code uses `D::begin()` directly. |
| 29 | 12 | Added Key Invariant #9: no backend name in generic types — use `D: DatabaseDriver` type parameters. Backend names allowed only in driver/dialect implementors and backward-compatible aliases. |

---

## 20. v0.18.0 Implementation State

> **Updated**: 2026-03-29

### Phase 1 — Foundation: Implemented

- `zeph-db` crate created at `crates/zeph-db/`
- `DatabaseDriver` trait, `Dialect` trait, `SqliteDriver`, `PostgresDriver` implemented
- `sql!()` macro for placeholder normalization implemented
- `DbConfig`, `redact_url()`, `FullDriver` blanket trait implemented
- All 49 SQLite migrations moved to `crates/zeph-db/migrations/sqlite/`
- `zeph-memory`, `zeph-scheduler`, `zeph-mcp`, `zeph-orchestration` depend on `zeph-db`
- `SqliteStore` → `DbStore` rename with backward-compatible alias

### Phase 2 — PostgreSQL Backend: Implemented

- 52 PostgreSQL migrations in `crates/zeph-db/migrations/postgres/`
  (49 base + 3 for agent identity and new v0.18.0 tables)
- `postgres` feature flag wired through workspace
- PostgreSQL FTS (`tsvector`/`tsquery`/GIN) implemented in `fts.rs`
- `begin_write()` uses `SELECT ... FOR UPDATE` on PostgreSQL for skill trust updates
- Integration tests with testcontainers behind `#[ignore]` flag

### Phase 3 — Config and Tooling: Implemented

- `MemoryConfig::database_url` field added (replaces `sqlite_path`, migration alias provided)
- `database_backend = "sqlite" | "postgres"` config toggle in `[memory]`
- `ZEPH_DATABASE_URL` vault key resolution for PostgreSQL credentials
- `--migrate-config` step renames `sqlite_path` to `database_url`
- `zeph db migrate` CLI subcommand for manual migration control
- `--init` wizard offers SQLite vs PostgreSQL backend selection
- Docker Compose env vars for local PostgreSQL testing:
  - `POSTGRES_DB=zeph_test`
  - `POSTGRES_USER=zeph`
  - `POSTGRES_PASSWORD=zeph`
  - `DATABASE_URL=postgres://zeph:zeph@localhost:5432/zeph_test`

### Phases 4+ — Remaining

- Agent identity (`agent_id` columns, migration 050) — planned
- `pgvector` integration for PostgreSQL-native vector storage — planned
- CI PostgreSQL matrix job — planned
