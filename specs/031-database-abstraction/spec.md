---
aliases:
  - Database Abstraction
  - PostgreSQL Backend
  - zeph-db Crate
tags:
  - sdd
  - spec
  - database
  - persistence
  - postgres
  - infra
created: 2026-03-28
status: approved
related:
  - "[[MOC-specs]]"
  - "[[004-memory/spec]]"
  - "[[018-scheduler/spec]]"
  - "[[001-system-invariants/spec#13. Database Backend Contract]]"
---

# Database Abstraction Layer: Multi-Backend Support (SQLite + PostgreSQL)

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
- Online migration between backends.
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

## 3. Architectural Decision: Generics + Traits with Feature-Flag Selection

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
`ActiveDriver` type alias. Only SQL fragments (via `Dialect` associated constants)
and pool construction differ per backend.

This approach achieves the goal stated in Section 1 with:
- Zero runtime overhead (monomorphization).
- Compile-time guarantees: if it builds with `--features postgres`, all queries
  are valid for PostgreSQL.
- Preserves SQLite-specific optimizations (WAL, PRAGMA, FTS5) when `sqlite` is
  selected.
- Allows PostgreSQL-specific optimizations (JSONB, GIN, pg_trgm) when `postgres`
  is selected.
- Dialect-specific SQL fragments provided by the `Dialect` trait, avoiding query duplication.

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
`--all-features` is not a supported build mode for this workspace. `full` itself is
backend-agnostic (does not include `sqlite` or `postgres`); use plain `--features full`
(relies on the default `sqlite`) for a SQLite build, or `--no-default-features --features
full,postgres` for a PostgreSQL build — default features must be disabled explicitly for
PostgreSQL because Cargo features are additive-only, so the default `sqlite` cannot be
"overridden" by requesting `postgres` on top of it. This is documented in CI
configuration (see also §4.2 amendment at line ~1129 on the `--no-default-features`
requirement, and the full-workspace `default-features = false` audit required across
every crate transitively depending on `zeph-db`/`zeph-memory` for this to compile
cleanly).

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
full = [...]  # backend-agnostic; does not include sqlite or postgres
postgres = ["zeph-db/postgres"]
# NOTE: --all-features activates both sqlite and postgres, triggering compile_error!.
# This is intentional. Use --features full (sqlite, via default) or
# --no-default-features --features full,postgres.
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
existing consumer code compiles unchanged. Consumer crates progressively adopt `Store<D>` generics where multi-backend support is needed.

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
```

**Amendment 3 [2026-07-12]** (#5970): The `max_connections`/`write_pool_size` two-field design
above was never actually implemented this way — the real `DbConfig` (see
`crates/zeph-db/src/pool.rs`) shipped with `max_connections` and `pool_size` fields whose
semantics contradicted both their names and this spec: `pool_size` (documented "SQLite only")
was what `connect_postgres` actually passed to `.max_connections()`, and `SQLite` combined the
two as `max_connections.max(pool_size)` — the larger value won, not a cap. `DbConfig` now has a
single `pool_size: u32` field, applied as `.max_connections()` for both backends. `max_connections`
is removed entirely (pre-v1.0.0, no deprecation shim). It was never a user-facing `config.toml`
key, so this is an internal-API-only breaking change.

```rust
/// Configuration for database pool construction.
pub struct DbConfig {
    /// Database URL. SQLite: file path or `:memory:`. Postgres: connection URL.
    pub url: String,
    /// Maximum number of connections in the pool, applied uniformly to both backends'
    /// `.max_connections()` builder call. Default 5.
    pub pool_size: u32,
}
```

```rust
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
required**: every `begin_write()` call site in `skills.rs` must be rewritten to
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
sqlx = { workspace = true, features = ["runtime-tokio", "tls-rustls", "migrate"] }
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
# REMOVE: sqlx = { workspace = true, features = ["macros", "runtime-tokio", "tls-rustls", "sqlite", "migrate"] }
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

> [!note] Superseded by the current constitution
> This subsection is the original RFC proposal and is historical. The constitution has since
> introduced sub-tiers within Layer 0 (0a/0b/0c); `zeph-db` is currently classified as **Layer 0b**
> ("depends on L0a only": `zeph-common`, `zeph-commands`) per [[constitution#I. Architecture]], not
> a bare "Layer 0" with no `zeph-*` deps — `crates/zeph-db/Cargo.toml` depends on `zeph-common`.
> The "constitution should be amended" suggestion below was resolved by that 0a/0b/0c split rather
> than by treating `zeph-db` as same-layer-exempt.

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

### Foundation Stage (non-breaking)

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

### PostgreSQL Backend Stage

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

### Config and Tooling Stage

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
reading and updating skill trust scores. This is a required implementation
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

**Mitigation**: A type alias `pub type SqliteStore = DbStore` provides
backward compatibility. Callers are migrated incrementally. The alias is removed
after all callers are updated (separate PR).

**Amendment 3 [2026-07-06]**: The migration above never took hold in practice —
`SqliteStore` remained the dominant name at ~485 call sites vs. 44 for `DbStore`
across the workspace, pre-1.0.0. Issue #5550 reverses course: the concrete
struct is renamed `DbStore` → `SqliteStore`, the `SqliteStore = DbStore` alias
is removed, and its 44 call sites are migrated to `SqliteStore` directly. This
is a one-off named exception for `SqliteStore` (see Key Invariant #9 below) —
it does not change the general mitigation strategy: type-alias-based incremental
migration remains the standard approach for any *future* backend-generic rename.

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
   (c) backward-compatible type aliases (`SqliteVectorStore`) — see Amendment 3 below
   for the `SqliteStore` exception.
   New code must use the generic forms.

   **Amendment 3 [2026-07-06]**: Exception for `SqliteStore` (issue #5550) — the
   concrete generic store type itself is renamed `DbStore` → `SqliteStore`, so the
   backend name now appears in (d) the primary generic store type's public name,
   not only in a backward-compatible alias. This is a single named exception,
   granted because `SqliteStore` was already the empirically dominant name in
   practice (~485 vs 44 call sites) and the incremental migration toward `DbStore`
   never took hold. It does **not** relax Invariant 9 in general: `DbVectorStore<D>`,
   `Store<D>`, and any future generic store/driver type must still avoid embedding
   backend names, per the original rule and the `Never` list in §13.

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
- Renaming `SqliteStore` to `DbStore`. **Resolved [2026-07-06]**: reversed by
  explicit user decision under issue #5550 — the concrete struct is renamed
  back `DbStore` → `SqliteStore` and the alias is removed (see §11.6 Amendment 3,
  Key Invariant #9 Amendment 3). Any *further* rename of the concrete generic
  store type still requires asking first.
- Amending the constitution to allow same-layer infrastructure crate imports.

### Never
- Use `sqlx::Any` backend.
- Remove SQLite support or make PostgreSQL the default.
- Introduce `openssl-sys` via `sqlx/tls-native-tls` feature.
- Mix placeholder styles (`?` and `$N`) in the same query string.
- Embed backend names ("Sqlite", "Postgres") in new generic type names
  (Invariant 9). The `SqliteStore` exception (Invariant 9, Amendment 3) is a
  single named, grandfathered case — not a license to introduce further
  backend-named generic types.

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
  Should these be consolidated into the `zeph-db` migration pipeline, or
  left as-is for now?

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

CI adds a separate job that runs these tests against a service
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
    use zeph_memory::SqliteStore;
    // ... use pg_pool from zeph-db test fixture via re-export or inline

    #[tokio::test]
    #[ignore = "requires docker"]
    async fn message_crud_roundtrip() {
        let (pool, _container) = zeph_db::test_utils::pg_pool().await;
        let store = SqliteStore::from_pool(pool);

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

## 18. Agent Identity in the Shared Data Model — [EXTRACTED]

**See `[[085-agent-identity-data-isolation/spec]]` for the complete agent-identity data isolation subsystem, extracted from this spec on 2026-07-27.**

This section previously contained the full specification of agent identity management, multi-tenant database isolation, schema changes, configuration design, and concurrent migration safety. All content has been moved to the dedicated specification above to reduce the scope of this document and improve modularity.


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
| 5 | 4.7, 11.2 | F8 (perf) | Mandated `SELECT ... FOR UPDATE` on PostgreSQL for skill trust score updates (the two `BEGIN IMMEDIATE` locations). Required implementation step. |
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

### Amendment 3: `SqliteStore` Rename Reversal (issue #5550) [2026-07-06]

Not triggered by a review artifact — an explicit user decision to reverse the
Amendment 2 migration direction. Pre-1.0.0, `SqliteStore` was empirically
dominant in practice (~485 vs 44 call sites for `DbStore`), and the incremental
migration toward `DbStore` mandated by Amendment 2 never took hold. Consolidating
on the dominant name was judged preferable to completing a migration nobody was
actually following.

| # | Section(s) | Change |
|---|-----------|--------|
| 30 | 11.6 | Reversed the `SqliteStore`/`DbStore` mitigation: the concrete struct is renamed `DbStore` → `SqliteStore`, the alias is removed, and all 44 `DbStore` call sites are migrated to `SqliteStore`. |
| 31 | 12 (Key Invariant #9) | Added a single named exception: `SqliteStore` may embed the backend name in the primary generic store type itself, not only in a backward-compatible alias. Scoped to `SqliteStore` only — all other generic store/driver types still follow the original rule. |
| 32 | 13 (Ask First, Never) | Marked the "Renaming `SqliteStore` to `DbStore`" Ask-First entry Resolved (direction reversed). Clarified the Never-list backend-name entry: the `SqliteStore` exception does not license further backend-named generic types. |

---

## 20. Implementation State

> **Updated**: 2026-03-29

### Foundation Stage: Implemented

- `zeph-db` crate created at `crates/zeph-db/`
- `DatabaseDriver` trait, `Dialect` trait, `SqliteDriver`, `PostgresDriver` implemented
- `sql!()` macro for placeholder normalization implemented
- `DbConfig`, `redact_url()`, `FullDriver` blanket trait implemented
- All 49 SQLite migrations moved to `crates/zeph-db/migrations/sqlite/`
- `zeph-memory`, `zeph-scheduler`, `zeph-mcp`, `zeph-orchestration` depend on `zeph-db`
- `SqliteStore` → `DbStore` rename with backward-compatible alias (reversed 2026-07-06 per Amendment 3 in §19 — the concrete struct is now `SqliteStore`, alias removed; see #5550)

### PostgreSQL Backend Stage: Implemented

- 52 PostgreSQL migrations in `crates/zeph-db/migrations/postgres/`
  (49 base + 3 for agent identity and related tables)
- `postgres` feature flag wired through workspace
- PostgreSQL FTS (`tsvector`/`tsquery`/GIN) implemented in `fts.rs`
- `begin_write()` uses `SELECT ... FOR UPDATE` on PostgreSQL for skill trust updates
- Integration tests with testcontainers behind `#[ignore]` flag

### Config and Tooling Stage: Implemented

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
